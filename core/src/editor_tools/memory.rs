//! What a tab keeps between calls. One [`Memory`] is shared by every call of a window, and goes when the
//! tab's line in the broker goes.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;

use super::proposals::{Proposal, MAX_PROPOSALS};
use super::tools::baseline::TurnBaseline;
use crate::turn_review::{ProposalWriter, ProposedFile};

/// How many exec calls a tab remembers. A replay of anything older is a new card, never a run with no card.
const EXEC_SEEN_MAX: usize = 256;

#[derive(Clone, Default)]
pub struct Memory {
    tabs: Rc<RefCell<BTreeMap<u64, TabMemory>>>,
    /// Where this window's proposals keep their bytes, as of the newest call.
    writer: Rc<RefCell<Option<ProposalWriter>>>,
}

#[derive(Default)]
pub struct TabMemory {
    /// What the tab's diagnostics were when its newest turn began.
    pub baseline: Option<TurnBaseline>,
    /// The exec calls already taken, oldest first, each by a digest of the agent's key for it.
    exec_seen: VecDeque<[u8; 32]>,
    /// The proposals still open to an apply, oldest first. They go with the tab's memory, so none crosses a move or
    /// outlives the session it was computed for.
    pub proposals: VecDeque<Rc<Proposal>>,
    /// The number of the tab's newest proposal.
    pub next_proposal: u64,
}

impl TabMemory {
    /// Records `key`; false when it is already here, so one call of the agent's raises one card.
    pub fn first_exec_of(&mut self, key: [u8; 32]) -> bool {
        if self.exec_seen.contains(&key) {
            return false;
        }
        if self.exec_seen.len() == EXEC_SEEN_MAX {
            self.exec_seen.pop_front();
        }
        self.exec_seen.push_back(key);
        true
    }
}

impl Memory {
    /// Runs `f` on the tab's memory, made empty if the tab has none. `f` may not reach this `Memory` again.
    pub fn with<R>(&self, tab: u64, f: impl FnOnce(&mut TabMemory) -> R) -> R {
        f(self.tabs.borrow_mut().entry(tab).or_default())
    }

    pub fn forget(&self, tab: u64) {
        self.tabs.borrow_mut().remove(&tab);
    }

    pub fn set_writer(&self, writer: Option<ProposalWriter>) {
        *self.writer.borrow_mut() = writer;
    }

    pub fn writer(&self) -> Option<ProposalWriter> {
        self.writer.borrow().clone()
    }

    /// Keeps a proposal under a new id of the tab, `p-<n>`. Past [`MAX_PROPOSALS`] the oldest goes, so an agent that
    /// never applies one holds a bounded amount.
    pub fn keep_proposal(&self, tab: u64, what: String, files: Vec<ProposedFile>) -> String {
        self.with(tab, |memory| {
            memory.next_proposal += 1;
            let id = format!("p-{}", memory.next_proposal);
            memory.proposals.push_back(Rc::new(Proposal {
                id: id.clone(),
                what,
                files,
            }));
            while memory.proposals.len() > MAX_PROPOSALS {
                memory.proposals.pop_front();
            }
            id
        })
    }

    pub fn proposal(&self, tab: u64, id: &str) -> Option<Rc<Proposal>> {
        let tabs = self.tabs.borrow();
        let proposals = &tabs.get(&tab)?.proposals;
        proposals.iter().find(|proposal| proposal.id == id).cloned()
    }

    /// The proposal has been used: its id answers nothing from now on.
    pub fn spend_proposal(&self, tab: u64, id: &str) {
        if let Some(memory) = self.tabs.borrow_mut().get_mut(&tab) {
            memory.proposals.retain(|proposal| proposal.id != id);
        }
    }

    /// How a test sees that a tab's memory went, whatever it held.
    #[cfg(test)]
    pub(crate) fn holds(&self, tab: u64) -> bool {
        self.tabs.borrow().contains_key(&tab)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tab_is_kept_until_it_is_forgotten() {
        let memory = Memory::default();
        assert_eq!(memory.with(1, |_| "ran"), "ran");
        memory.clone().with(2, |_| ());
        assert_eq!(memory.tabs.borrow().len(), 2, "a clone holds the same tabs");
        memory.forget(1);
        memory.forget(9);
        assert_eq!(memory.tabs.borrow().keys().copied().collect::<Vec<_>>(), vec![2]);
    }

    fn key(n: u32) -> [u8; 32] {
        let mut key = [0; 32];
        key[..4].copy_from_slice(&n.to_le_bytes());
        key
    }

    fn keep(memory: &Memory, tab: u64) -> String {
        memory.keep_proposal(tab, "a change".to_owned(), Vec::new())
    }

    #[test]
    fn the_oldest_proposal_goes_past_eight() {
        let memory = Memory::default();
        let ids: Vec<String> = (0..MAX_PROPOSALS).map(|_| keep(&memory, 1)).collect();
        assert_eq!(ids[0], "p-1");
        assert_eq!(ids[MAX_PROPOSALS - 1], format!("p-{MAX_PROPOSALS}"));
        assert!(memory.proposal(1, "p-1").is_some(), "still held at the limit");

        let newest = keep(&memory, 1);
        assert!(memory.proposal(1, "p-1").is_none(), "the oldest went");
        assert!(memory.proposal(1, "p-2").is_some(), "the next stayed");
        assert_eq!(memory.proposal(1, &newest).unwrap().what, "a change");
    }

    #[test]
    fn a_spent_proposal_answers_nothing_and_its_number_is_not_reused() {
        let memory = Memory::default();
        let first = keep(&memory, 1);
        memory.spend_proposal(1, &first);
        memory.spend_proposal(9, &first);
        assert!(memory.proposal(1, &first).is_none());
        assert_eq!(keep(&memory, 1), "p-2");
    }

    #[test]
    fn a_proposal_belongs_to_its_tab() {
        let memory = Memory::default();
        let id = keep(&memory, 1);
        assert!(memory.proposal(2, &id).is_none());
        assert!(!memory.holds(2), "looking made no memory for the tab");
        assert_eq!(keep(&memory, 2), "p-1");
    }

    #[test]
    fn a_tab_forgotten_loses_its_proposals() {
        let memory = Memory::default();
        let id = keep(&memory, 1);
        memory.forget(1);
        assert!(memory.proposal(1, &id).is_none());
    }

    #[test]
    fn a_clone_shares_the_writer() {
        let memory = Memory::default();
        assert!(memory.writer().is_none());
        memory
            .clone()
            .set_writer(Some(ProposalWriter::new(None, std::path::Path::new("/p"))));
        assert!(memory.writer().is_some());
        memory.set_writer(None);
        assert!(memory.writer().is_none());
    }

    #[test]
    fn an_exec_key_counts_once() {
        let mut tab = TabMemory::default();
        assert!(tab.first_exec_of(key(1)));
        assert!(!tab.first_exec_of(key(1)));
        assert!(tab.first_exec_of(key(2)));
    }

    #[test]
    fn the_oldest_exec_key_goes_past_the_limit() {
        let mut tab = TabMemory::default();
        for n in 0..EXEC_SEEN_MAX as u32 {
            assert!(tab.first_exec_of(key(n)));
        }
        assert!(!tab.first_exec_of(key(0)), "still held at the limit");
        assert!(tab.first_exec_of(key(EXEC_SEEN_MAX as u32)));
        assert_eq!(tab.exec_seen.len(), EXEC_SEEN_MAX);
        assert!(tab.first_exec_of(key(0)), "the oldest went");
        assert!(!tab.first_exec_of(key(2)), "the rest stayed");
    }
}
