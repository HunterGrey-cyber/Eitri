//! A tab's thread and the subagent threads that descend from it.
//!
//! Codex cannot be told not to spawn subagents, and a child keeps working on its own turn after its
//! parent's turn has ended. The tab's turn therefore has to last as long as any thread of the family has a
//! turn running, or a revert, a review snapshot and the queue would all treat the tab as idle while a child
//! is still writing files.
//!
//! This module holds only the bookkeeping: which threads belong, what each is called, and which turns are
//! running. It reads and writes nothing; the provider feeds it what Codex reported and acts on the edge it
//! returns.

use super::wire::{SubAgentActivityItem, SubAgentKind};
use std::collections::HashMap;

/// Threads remembered as members at most, so a flood of announcements cannot grow without bound.
const MEMBERS_MAX: usize = 256;

/// Stands for the turn of a member thread whose start could not be read. Codex never gives a turn an
/// empty id, and the first readable message naming a turn on that thread says which turn it is
/// ([`Family::adopt`]).
pub const UNREAD_TURN: &str = "";

/// What a start or an end of a family turn did to the tab's turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FamilyEdge {
    /// The tab was idle and is now busy: its turn begins.
    Opened,
    /// The tab was already busy; the turn that started belongs to the one running.
    Joined,
    /// The last running turn of the family ended: the tab's turn ends.
    Closed,
    /// A family turn ended and others are still running.
    Still,
    /// Nothing changed: the thread is not a member, or the turn was not the one running on it.
    Ignored,
}

#[derive(Debug)]
pub struct Family {
    /// The tab's own thread, the root of the family.
    tab_thread: String,
    /// Subagent threads by id, with the task name Codex gave each (`/root/child`).
    announced: HashMap<String, String>,
    /// The running turn of each member thread, in the order the turns started.
    running: Vec<(String, String)>,
    /// A tab turn is open: some turn of the family has started and the family has not been idle since.
    busy: bool,
}

impl Family {
    pub fn new(tab_thread: impl Into<String>) -> Family {
        Family {
            tab_thread: tab_thread.into(),
            announced: HashMap::new(),
            running: Vec::new(),
            busy: false,
        }
    }

    /// The tab's own thread.
    pub fn tab_thread(&self) -> &str {
        &self.tab_thread
    }

    /// Whether `thread` is the tab's thread or one announced from the family.
    pub fn contains(&self, thread: &str) -> bool {
        thread == self.tab_thread || self.announced.contains_key(thread)
    }

    /// Takes a `subAgentActivity` item seen on `on_thread` into account. Only a `started` item that arrives
    /// on a member thread adds a thread (a child's own children included); an announcement on a thread the
    /// tab does not follow is refused. Returns whether the item named a member after the call.
    pub fn announce(&mut self, on_thread: &str, item: &SubAgentActivityItem) -> bool {
        if !self.contains(on_thread) || item.kind != SubAgentKind::Started {
            return false;
        }
        if item.agent_thread_id == self.tab_thread {
            return true;
        }
        if !self.announced.contains_key(&item.agent_thread_id) && self.announced.len() >= MEMBERS_MAX {
            return false;
        }
        self.announced
            .insert(item.agent_thread_id.clone(), item.agent_path.clone());
        true
    }

    /// Takes the receivers of a `collabAgentToolCall` seen on `on_thread` into account. Under a model with the
    /// plain tool set Codex announces a spawned child only this way (the call's end names it), never with a
    /// `subAgentActivity`; a call that sends a member input or resumes it names it too. Only a call on a
    /// member thread adds threads, under the same cap as an announcement. A receiver gets a label of the
    /// calling thread's path and the end of its own id, since the call carries no task name; a later
    /// `subAgentActivity` for it replaces that with its path. Returns the threads that joined.
    pub fn announce_receivers(&mut self, on_thread: &str, receivers: &[String]) -> Vec<String> {
        if !self.contains(on_thread) {
            return Vec::new();
        }
        let parent = self.label(on_thread).unwrap_or("/root").to_string();
        let mut joined = Vec::new();
        for receiver in receivers {
            if receiver.is_empty() || self.contains(receiver) {
                continue;
            }
            if self.announced.len() >= MEMBERS_MAX {
                break;
            }
            let start = receiver.char_indices().rev().nth(7).map_or(0, |(at, _)| at);
            self.announced
                .insert(receiver.clone(), format!("{parent}/{}", &receiver[start..]));
            joined.push(receiver.clone());
        }
        joined
    }

    /// A turn started on a member thread.
    pub fn turn_started(&mut self, thread: &str, turn: &str) -> FamilyEdge {
        if !self.contains(thread) {
            return FamilyEdge::Ignored;
        }
        match self.running.iter_mut().find(|(t, _)| t == thread) {
            // The same turn reported twice (the answer to `turn/start`, then its notification).
            Some((_, running)) if running == turn => return FamilyEdge::Joined,
            // A new turn on a thread whose old one never reported its end: the new one is the running one.
            Some((_, running)) => *running = turn.to_string(),
            None => self.running.push((thread.to_string(), turn.to_string())),
        }
        if self.busy {
            FamilyEdge::Joined
        } else {
            self.busy = true;
            FamilyEdge::Opened
        }
    }

    /// The turn running on a member thread ended. `Closed` once nothing of the family is running.
    pub fn turn_completed(&mut self, thread: &str, turn: &str) -> FamilyEdge {
        let Some(at) = self.running.iter().position(|(t, r)| t == thread && r == turn) else {
            return FamilyEdge::Ignored;
        };
        self.running.remove(at);
        if self.running.is_empty() && self.busy {
            self.busy = false;
            FamilyEdge::Closed
        } else {
            FamilyEdge::Still
        }
    }

    /// The turn running on `thread` is one whose start could not be read: `turn`, the first turn a
    /// readable message names there, is that one. Returns whether it was taken.
    pub fn adopt(&mut self, thread: &str, turn: &str) -> bool {
        if turn == UNREAD_TURN {
            return false;
        }
        match self.running.iter_mut().find(|(t, r)| t == thread && r == UNREAD_TURN) {
            Some((_, running)) => {
                *running = turn.to_string();
                true
            }
            None => false,
        }
    }

    /// Whether `turn` is the one running on `thread`.
    pub fn is_running(&self, thread: &str, turn: &str) -> bool {
        self.running.iter().any(|(t, r)| t == thread && r == turn)
    }

    /// The turn running on `thread`, if any.
    pub fn running_on(&self, thread: &str) -> Option<&str> {
        self.running
            .iter()
            .find(|(t, _)| t == thread)
            .map(|(_, turn)| turn.as_str())
    }

    /// Every running turn as `(thread, turn)`, in the order they started.
    pub fn running_turns(&self) -> Vec<(String, String)> {
        self.running.clone()
    }

    /// The task name of a subagent thread (`/root/child`); the tab's own thread has none.
    pub fn label(&self, thread: &str) -> Option<&str> {
        self.announced.get(thread).map(String::as_str)
    }

    /// Forgets every running turn: the tab's turn was ended by something other than the family going idle.
    /// Members stay, since a child outlives a turn and may start another.
    pub fn reset_turns(&mut self) {
        self.running.clear();
        self.busy = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn started(thread: &str, path: &str) -> SubAgentActivityItem {
        SubAgentActivityItem {
            id: "x".into(),
            kind: SubAgentKind::Started,
            agent_thread_id: thread.into(),
            agent_path: path.into(),
        }
    }

    #[test]
    fn members_are_the_tab_and_what_its_family_announced() {
        let mut family = Family::new("tab");
        assert!(family.announce("tab", &started("child", "/root/child")));
        assert!(family.announce("child", &started("grand", "/root/child/grand")));
        assert!(!family.announce("stranger", &started("other", "/root/other")));
        assert!(family.contains("grand"));
        assert!(!family.contains("other"));
        assert_eq!(family.label("grand"), Some("/root/child/grand"));
        assert_eq!(family.label("tab"), None);
    }

    #[test]
    fn collab_receivers_join_from_member_threads_only() {
        let mut family = Family::new("tab");
        let child = "01a11b94-eb86-7d10-bd77-2ac835206b32".to_string();
        assert_eq!(family.announce_receivers("tab", &[child.clone()]), vec![child.clone()]);
        assert_eq!(family.label(&child), Some("/root/35206b32"));
        assert!(
            family.announce_receivers("tab", &[child.clone()]).is_empty(),
            "already a member"
        );
        assert_eq!(
            family.announce_receivers(&child, &["grand".to_string()]),
            vec!["grand".to_string()]
        );
        assert_eq!(family.label("grand"), Some("/root/35206b32/grand"));
        assert!(family.announce_receivers("stranger", &["ghost".to_string()]).is_empty());
        assert!(!family.contains("ghost"));
        // An announcement with a path names the thread properly from then on.
        family.announce("tab", &started(&child, "/root/child"));
        assert_eq!(family.label(&child), Some("/root/child"));
    }

    #[test]
    fn collab_receivers_respect_the_member_cap() {
        let mut family = Family::new("tab");
        let many: Vec<String> = (0..MEMBERS_MAX + 10).map(|n| format!("t{n}")).collect();
        assert_eq!(family.announce_receivers("tab", &many).len(), MEMBERS_MAX);
        assert!(!family.contains(&format!("t{}", MEMBERS_MAX)));
        assert!(!family.announce("tab", &started("late", "/root/late")));
    }

    #[test]
    fn only_a_started_item_adds_a_member() {
        let mut family = Family::new("tab");
        let mut item = started("child", "/root/child");
        item.kind = SubAgentKind::Completed;
        assert!(!family.announce("tab", &item));
        assert!(!family.contains("child"));
    }

    #[test]
    fn the_tab_turn_closes_with_the_last_running_turn() {
        let mut family = Family::new("tab");
        family.announce("tab", &started("child", "/root/child"));
        assert_eq!(family.turn_started("tab", "t1"), FamilyEdge::Opened);
        assert_eq!(family.turn_started("tab", "t1"), FamilyEdge::Joined);
        assert_eq!(family.turn_started("child", "c1"), FamilyEdge::Joined);
        assert_eq!(family.turn_completed("tab", "t1"), FamilyEdge::Still);
        assert_eq!(family.turn_completed("child", "other"), FamilyEdge::Ignored);
        assert_eq!(family.turn_completed("child", "c1"), FamilyEdge::Closed);
        assert_eq!(family.turn_completed("child", "c1"), FamilyEdge::Ignored);
        // A child that works on after the tab went idle opens a new tab turn.
        assert_eq!(family.turn_started("child", "c2"), FamilyEdge::Opened);
    }

    #[test]
    fn an_unread_start_is_taken_by_the_first_turn_named() {
        let mut family = Family::new("tab");
        family.announce("tab", &started("child", "/root/child"));
        assert_eq!(family.turn_started("tab", "t1"), FamilyEdge::Opened);
        assert_eq!(family.turn_started("child", UNREAD_TURN), FamilyEdge::Joined);
        assert!(!family.adopt("tab", "t9"), "the tab's turn was read");
        assert!(family.adopt("child", "c1"));
        assert!(!family.adopt("child", "c2"), "only the unread one is taken");
        assert!(family.is_running("child", "c1"));
        assert_eq!(family.turn_completed("tab", "t1"), FamilyEdge::Still);
        assert_eq!(family.turn_completed("child", "c1"), FamilyEdge::Closed);
    }

    #[test]
    fn a_thread_outside_the_family_changes_nothing() {
        let mut family = Family::new("tab");
        assert_eq!(family.turn_started("stranger", "s1"), FamilyEdge::Ignored);
        assert!(family.running_turns().is_empty());
    }

    #[test]
    fn running_turns_come_in_start_order() {
        let mut family = Family::new("tab");
        family.announce("tab", &started("a", "/root/a"));
        family.announce("tab", &started("b", "/root/b"));
        family.turn_started("tab", "t");
        family.turn_started("b", "tb");
        family.turn_started("a", "ta");
        assert_eq!(
            family.running_turns(),
            vec![
                ("tab".into(), "t".into()),
                ("b".into(), "tb".into()),
                ("a".into(), "ta".into())
            ]
        );
    }
}
