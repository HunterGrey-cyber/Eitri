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
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};

/// Threads remembered as members at most, so a flood of announcements cannot grow without bound.
pub const MEMBERS_MAX: usize = 256;

/// Turns remembered as over per thread, newest kept.
const ENDED_KEPT: usize = 8;

/// A thread the family would have to take in is past [`MEMBERS_MAX`]. It cannot be left out: a subagent
/// that is not followed can write files while the tab reads as idle, so the caller ends the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Full;

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

#[derive(Debug, Serialize, Deserialize)]
pub struct Family {
    /// The tab's own thread, the root of the family.
    tab_thread: String,
    /// Subagent threads by id, with the task name Codex gave each (`/root/child`).
    announced: HashMap<String, String>,
    /// The running turn of each member thread, in the order the turns started.
    running: Vec<(String, String)>,
    /// The last turns of each thread that are over, newest last. A late message about one of them is not
    /// the turn of that thread that runs now.
    ended: HashMap<String, VecDeque<String>>,
    /// A tab turn is open: some turn of the family has started and the family has not been idle since.
    busy: bool,
}

impl Family {
    pub fn new(tab_thread: impl Into<String>) -> Family {
        Family {
            tab_thread: tab_thread.into(),
            announced: HashMap::new(),
            running: Vec::new(),
            ended: HashMap::new(),
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
    /// tab does not follow is refused. Returns whether the item named a member after the call, or [`Full`]
    /// when it named a new thread past the cap.
    pub fn announce(&mut self, on_thread: &str, item: &SubAgentActivityItem) -> Result<bool, Full> {
        // Codex never names a thread with an empty id; taking one in would make every message that lacks
        // a thread read as the family's.
        if !self.contains(on_thread) || item.kind != SubAgentKind::Started || item.agent_thread_id.is_empty() {
            return Ok(false);
        }
        if item.agent_thread_id == self.tab_thread {
            return Ok(true);
        }
        if !self.announced.contains_key(&item.agent_thread_id) && self.announced.len() >= MEMBERS_MAX {
            return Err(Full);
        }
        self.announced
            .insert(item.agent_thread_id.clone(), item.agent_path.clone());
        Ok(true)
    }

    /// Takes the receivers of a `collabAgentToolCall` seen on `on_thread` into account. Under a model with the
    /// plain tool set Codex announces a spawned child only this way (the call's end names it), never with a
    /// `subAgentActivity`; a call that sends a member input or resumes it names it too. Only a call on a
    /// member thread adds threads, under the same cap as an announcement. A receiver gets a label of the
    /// calling thread's path and the end of its own id, since the call carries no task name; a later
    /// `subAgentActivity` for it replaces that with its path. Returns the threads that joined, or [`Full`]
    /// when a receiver would pass the cap.
    pub fn announce_receivers(&mut self, on_thread: &str, receivers: &[String]) -> Result<Vec<String>, Full> {
        if !self.contains(on_thread) {
            return Ok(Vec::new());
        }
        let parent = self.label(on_thread).unwrap_or("/root").to_string();
        let mut joined = Vec::new();
        for receiver in receivers {
            if receiver.is_empty() || self.contains(receiver) {
                continue;
            }
            if self.announced.len() >= MEMBERS_MAX {
                return Err(Full);
            }
            let start = receiver.char_indices().rev().nth(7).map_or(0, |(at, _)| at);
            self.announced
                .insert(receiver.clone(), format!("{parent}/{}", &receiver[start..]));
            joined.push(receiver.clone());
        }
        Ok(joined)
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
            Some((_, running)) => {
                let old = std::mem::replace(running, turn.to_string());
                self.note_ended(thread, old);
            }
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
        let (thread, turn) = self.running.remove(at);
        self.note_ended(&thread, turn);
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
        // A turn of that thread that is already over is not the one running now: a late message about it
        // would otherwise give the stand-in the old turn's id, and the new turn would never be seen to end.
        if turn == UNREAD_TURN
            || self
                .ended
                .get(thread)
                .is_some_and(|ended| ended.iter().any(|t| t == turn))
        {
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

    /// Remembers that `turn` of `thread` is over.
    fn note_ended(&mut self, thread: &str, turn: String) {
        if turn == UNREAD_TURN {
            return;
        }
        let ended = self.ended.entry(thread.to_string()).or_default();
        if ended.len() >= ENDED_KEPT {
            ended.pop_front();
        }
        ended.push_back(turn);
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
        for (thread, turn) in std::mem::take(&mut self.running) {
            self.note_ended(&thread, turn);
        }
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
        assert_eq!(family.announce("tab", &started("child", "/root/child")), Ok(true));
        assert_eq!(
            family.announce("child", &started("grand", "/root/child/grand")),
            Ok(true)
        );
        assert_eq!(family.announce("stranger", &started("other", "/root/other")), Ok(false));
        assert!(family.contains("grand"));
        assert!(!family.contains("other"));
        assert_eq!(family.label("grand"), Some("/root/child/grand"));
        assert_eq!(family.label("tab"), None);
    }

    #[test]
    fn collab_receivers_join_from_member_threads_only() {
        let mut family = Family::new("tab");
        let child = "01a11b94-eb86-7d10-bd77-2ac835206b32".to_string();
        assert_eq!(
            family.announce_receivers("tab", &[child.clone()]),
            Ok(vec![child.clone()])
        );
        assert_eq!(family.label(&child), Some("/root/35206b32"));
        assert_eq!(
            family.announce_receivers("tab", &[child.clone()]),
            Ok(Vec::new()),
            "already a member"
        );
        assert_eq!(
            family.announce_receivers(&child, &["grand".to_string()]),
            Ok(vec!["grand".to_string()])
        );
        assert_eq!(family.label("grand"), Some("/root/35206b32/grand"));
        assert_eq!(
            family.announce_receivers("stranger", &["ghost".to_string()]),
            Ok(Vec::new())
        );
        assert!(!family.contains("ghost"));
        // An announcement with a path names the thread properly from then on.
        assert_eq!(family.announce("tab", &started(&child, "/root/child")), Ok(true));
        assert_eq!(family.label(&child), Some("/root/child"));
    }

    #[test]
    fn a_thread_past_the_member_cap_is_reported() {
        let mut family = Family::new("tab");
        let many: Vec<String> = (0..MEMBERS_MAX).map(|n| format!("t{n}")).collect();
        assert_eq!(
            family.announce_receivers("tab", &many).map(|j| j.len()),
            Ok(MEMBERS_MAX)
        );
        assert_eq!(family.announce_receivers("tab", &["one-more".to_string()]), Err(Full));
        assert!(!family.contains("one-more"));
        assert_eq!(family.announce("tab", &started("late", "/root/late")), Err(Full));
        // A member already in, or one named from outside the family, is no new thread.
        assert_eq!(family.announce("tab", &started("t0", "/root/t0")), Ok(true));
        assert_eq!(family.announce_receivers("tab", &["t1".to_string()]), Ok(Vec::new()));
        assert_eq!(
            family.announce_receivers("stranger", &["ghost".to_string()]),
            Ok(Vec::new())
        );
    }

    #[test]
    fn only_a_started_item_adds_a_member() {
        let mut family = Family::new("tab");
        let mut item = started("child", "/root/child");
        item.kind = SubAgentKind::Completed;
        assert_eq!(family.announce("tab", &item), Ok(false));
        assert!(!family.contains("child"));
    }

    #[test]
    fn the_tab_turn_closes_with_the_last_running_turn() {
        let mut family = Family::new("tab");
        assert_eq!(family.announce("tab", &started("child", "/root/child")), Ok(true));
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
        assert_eq!(family.announce("tab", &started("child", "/root/child")), Ok(true));
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
    fn an_unread_start_never_takes_a_turn_that_is_over() {
        let mut family = Family::new("tab");
        assert_eq!(family.announce("tab", &started("child", "/root/child")), Ok(true));
        assert_eq!(family.turn_started("child", "c1"), FamilyEdge::Opened);
        assert_eq!(family.turn_completed("child", "c1"), FamilyEdge::Closed);
        assert_eq!(family.turn_started("child", UNREAD_TURN), FamilyEdge::Opened);
        assert!(!family.adopt("child", "c1"), "a late message about the turn that ended");
        assert!(family.adopt("child", "c2"));
        assert_eq!(family.turn_completed("child", "c2"), FamilyEdge::Closed);
    }

    #[test]
    fn an_empty_thread_id_never_joins() {
        let mut family = Family::new("tab");
        assert_eq!(family.announce("tab", &started("", "/root/nameless")), Ok(false));
        assert!(!family.contains(""));
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
        assert_eq!(family.announce("tab", &started("a", "/root/a")), Ok(true));
        assert_eq!(family.announce("tab", &started("b", "/root/b")), Ok(true));
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
