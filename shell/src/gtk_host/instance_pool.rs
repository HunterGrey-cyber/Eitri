//! Live and idle module instances stay keyed by the id they were made for.

use eitri_core::layout::ModuleId;
use std::collections::BTreeMap;

pub(crate) trait Retire {
    fn on_retire(&self);
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Reused {
    Yes,
    No,
}

/// Entries never change ids, because a page's message handler keeps the view it was built for.
/// Reads need no retirement hook; inserting or retiring an entry requires its kind's hook.
pub(crate) struct InstancePool<P> {
    live: BTreeMap<ModuleId, P>,
    idle: BTreeMap<ModuleId, P>,
}

impl<P> InstancePool<P> {
    pub(crate) fn new() -> Self {
        Self {
            live: BTreeMap::new(),
            idle: BTreeMap::new(),
        }
    }

    pub(crate) fn get(&self, id: &ModuleId) -> Option<&P> {
        self.live.get(id)
    }

    pub(crate) fn live(&self) -> impl Iterator<Item = (&ModuleId, &P)> {
        self.live.iter()
    }

    pub(crate) fn each(&self) -> impl Iterator<Item = &P> {
        self.live.values().chain(self.idle.values())
    }
}

impl<P: Retire + Clone> InstancePool<P> {
    /// Reuse a live or idle entry only under its original id.
    pub(crate) fn take_or_make(&mut self, id: &ModuleId, make: impl FnOnce() -> P) -> (P, Reused) {
        if let Some(entry) = self.live.get(id) {
            return (entry.clone(), Reused::Yes);
        }
        let (entry, reused) = match self.idle.remove(id) {
            Some(entry) => (entry, Reused::Yes),
            None => (make(), Reused::No),
        };
        self.live.insert(id.clone(), entry.clone());
        (entry, reused)
    }

    /// The entry under `id` made live again: the live one as it is, else the idle one taken back. `None` when the
    /// pool never held one: only an entry made for `id` may come back under it.
    pub(crate) fn revive(&mut self, id: &ModuleId) -> Option<P> {
        if let Some(entry) = self.live.get(id) {
            return Some(entry.clone());
        }
        let entry = self.idle.remove(id)?;
        self.live.insert(id.clone(), entry.clone());
        Some(entry)
    }

    /// Run the kind's retirement hook once as the live entry moves to its idle slot.
    pub(crate) fn retire(&mut self, id: &ModuleId) -> bool {
        let Some(entry) = self.set_idle(id) else {
            return false;
        };
        entry.on_retire();
        true
    }

    /// The live entry under `id` moved to its idle slot without its retirement hook, which the caller runs on the
    /// entry answered once nothing holds the pool. `None` when no live entry has that id.
    pub(crate) fn set_idle(&mut self, id: &ModuleId) -> Option<P> {
        let entry = self.live.remove(id)?;
        self.idle.insert(id.clone(), entry.clone());
        Some(entry)
    }

    /// A new entry made for `id`, live from now on. Only for an id the pool holds no entry for, live or idle: an entry
    /// never changes ids.
    pub(crate) fn insert(&mut self, id: &ModuleId, entry: P) {
        debug_assert!(
            !self.live.contains_key(id) && !self.idle.contains_key(id),
            "{id} already has an entry"
        );
        self.live.insert(id.clone(), entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    type ChatViews<P> = InstancePool<P>;
    use eitri_core::layout::ModuleKind;

    /// A page that says which id it was made for, and which page it is among those made for that id.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Page {
        made_for: u8,
        serial: u32,
    }

    impl Retire for Page {
        fn on_retire(&self) {}
    }

    fn agent(n: u8) -> ModuleId {
        ModuleId::instance(ModuleKind::Agent, n).unwrap()
    }

    /// A pane a kill left idle comes back live under its own id only, once, and an id never made gets nothing.
    #[test]
    fn revive_takes_an_idle_entry_back_live_under_its_own_id() {
        let mut views = ChatViews::new();
        let (four, _) = views.take_or_make(&agent(4), || Page { made_for: 4, serial: 1 });
        assert_eq!(
            views.revive(&agent(4)),
            Some(four.clone()),
            "a live entry is answered as it is"
        );
        assert!(views.retire(&agent(4)));
        assert!(views.revive(&agent(2)).is_none(), "nothing was made for agent#2");
        assert_eq!(views.revive(&agent(4)), Some(four.clone()));
        assert_eq!(views.get(&agent(4)), Some(&four), "live again");
        assert!(views.retire(&agent(4)), "and retired again by the next kill");
    }

    /// `agent#4` killed goes idle under `agent#4`: a new `agent#2` gets a page of its own, and only a new
    /// `agent#4` takes the old page back.
    #[test]
    fn a_killed_view_goes_idle_under_its_own_id_and_only_that_id_reuses_it() {
        let mut views = ChatViews::new();
        let mut serial = 0;
        let mut make = |n: u8| {
            serial += 1;
            Page { made_for: n, serial }
        };
        let (four, reused) = views.take_or_make(&agent(4), || make(4));
        assert_eq!(reused, Reused::No);
        assert!(views.retire(&agent(4)));
        assert!(views.get(&agent(4)).is_none(), "an idle page is not live");
        assert!(!views.retire(&agent(4)), "a page goes idle once");
        assert_eq!(views.live().count(), 0);

        let (two, reused) = views.take_or_make(&agent(2), || make(2));
        assert_eq!(reused, Reused::No, "another number never takes agent#4's page");
        assert_eq!(two.made_for, 2);
        assert_ne!(two, four);

        let (again, reused) = views.take_or_make(&agent(4), || panic!("agent#4's idle page is reused"));
        assert_eq!(reused, Reused::Yes);
        assert_eq!(again, four);
        for (id, page) in views.live() {
            assert_eq!(
                *id,
                agent(page.made_for),
                "every live page is under the id it was made for"
            );
        }
        assert_eq!(views.live().count(), 2);
    }

    #[test]
    fn a_live_view_is_returned_as_it_is() {
        let mut views = ChatViews::new();
        let (first, reused) = views.take_or_make(&agent(3), || Page { made_for: 3, serial: 1 });
        assert_eq!(reused, Reused::No);
        let (second, reused) = views.take_or_make(&agent(3), || panic!("a live page is never made again"));
        assert_eq!(reused, Reused::Yes);
        assert_eq!(first, second);
        assert_eq!(views.get(&agent(3)), Some(&first));
        assert_eq!(views.live().count(), 1);
    }
}

#[cfg(test)]
mod terminal_tests {
    use super::*;
    type Terminals<P> = InstancePool<P>;
    use eitri_core::layout::ModuleKind;
    use std::cell::Cell;
    use std::rc::Rc;

    #[derive(Clone)]
    struct FakePane {
        serial: u8,
        closes: Rc<Cell<u32>>,
        generation: Rc<Cell<u64>>,
    }

    impl FakePane {
        fn new(serial: u8) -> Self {
            Self {
                serial,
                closes: Rc::new(Cell::new(0)),
                generation: Rc::new(Cell::new(0)),
            }
        }

        fn start(&self) {
            self.generation.set(self.generation.get() + 1);
        }
    }

    impl Retire for FakePane {
        fn on_retire(&self) {
            self.closes.set(self.closes.get() + 1);
        }
    }

    fn id(n: u8) -> ModuleId {
        ModuleId::instance(ModuleKind::Terminal, n).unwrap()
    }

    #[test]
    fn a_pooled_pane_is_reused_only_under_its_own_id() {
        let mut terminals = Terminals::new();
        for n in 2..=4 {
            let (pane, reused) = terminals.take_or_make(&id(n), || FakePane::new(n));
            assert_eq!(pane.serial, n);
            assert_eq!(reused, Reused::No);
        }
        for n in [3, 2] {
            assert!(terminals.retire(&id(n)));
            assert!(terminals.get(&id(n)).is_none());
            let (pane, reused) = terminals.take_or_make(&id(n), || panic!("must reuse its own pane"));
            assert_eq!(pane.serial, n);
            assert_eq!(reused, Reused::Yes);
        }
        assert!(terminals.retire(&id(3)));
        let (pane, reused) = terminals.take_or_make(&id(5), || FakePane::new(5));
        assert_eq!(pane.serial, 5);
        assert_eq!(reused, Reused::No);
        assert_eq!(terminals.live().count(), 3);
        assert_eq!(terminals.each().count(), 4);
        for (id, pane) in terminals.live() {
            assert_eq!(id, &self::id(pane.serial));
        }
        let (pane, _) = terminals.take_or_make(&id(4), || panic!("already live"));
        assert_eq!(pane.serial, 4);
    }

    #[test]
    fn killing_an_instance_closes_only_its_own_pane() {
        let mut terminals = Terminals::new();
        let panes: Vec<_> = (1..=3)
            .map(|n| terminals.take_or_make(&id(n), || FakePane::new(n)).0)
            .collect();
        assert!(terminals.retire(&id(2)));
        assert!(!terminals.retire(&id(2)));
        assert_eq!(panes.iter().map(|p| p.closes.get()).collect::<Vec<_>>(), [0, 1, 0]);
        assert_eq!(terminals.get(&id(1)).unwrap().serial, 1);
        assert_eq!(terminals.get(&id(3)).unwrap().serial, 3);
    }

    #[test]
    fn a_late_exit_from_a_killed_shell_never_closes_the_instance_that_reused_its_number() {
        use crate::terminal::exit_is_current;
        assert!(!exit_is_current(1, 2));
        assert!(exit_is_current(2, 2));
        let mut terminals = Terminals::new();
        let (pane, _) = terminals.take_or_make(&id(2), || FakePane::new(2));
        pane.start();
        let reported = pane.generation.get();
        assert!(terminals.retire(&id(2)));
        let (reopened, reused) = terminals.take_or_make(&id(2), || panic!("must reuse"));
        assert_eq!(reused, Reused::Yes);
        reopened.start();
        assert_eq!(reopened.generation.get(), 2);
        assert!(!exit_is_current(reported, reopened.generation.get()));
    }
}

#[cfg(test)]
mod retire_tests {
    use super::*;
    use eitri_core::layout::ModuleKind;
    use std::{cell::Cell, rc::Rc};

    #[derive(Clone)]
    struct RecordingTerminal(Rc<Cell<u32>>);
    impl RecordingTerminal {
        fn close(&self) {
            self.0.set(self.0.get() + 1);
        }
    }
    impl Retire for RecordingTerminal {
        fn on_retire(&self) {
            self.close();
        }
    }
    #[derive(Clone)]
    struct RecordingChat(Rc<Cell<u32>>);
    impl Retire for RecordingChat {
        fn on_retire(&self) {}
    }

    #[test]
    fn retiring_a_terminal_closes_its_pane_and_a_chat_page_is_only_kept() {
        let terminal = ModuleId::instance(ModuleKind::Terminal, 2).unwrap();
        let chat = ModuleId::instance(ModuleKind::Agent, 2).unwrap();
        let mut terminals = InstancePool::new();
        let mut chats = InstancePool::new();
        let (pane, _) = terminals.take_or_make(&terminal, || RecordingTerminal(Rc::new(Cell::new(0))));
        let (page, _) = chats.take_or_make(&chat, || RecordingChat(Rc::new(Cell::new(0))));
        assert!(terminals.retire(&terminal));
        assert!(!terminals.retire(&terminal));
        assert!(chats.retire(&chat));
        assert!(!chats.retire(&chat));
        assert_eq!(pane.0.get(), 1);
        assert_eq!(page.0.get(), 0);
        let (again, reused) = terminals.take_or_make(&terminal, || panic!("reuse terminal"));
        assert_eq!(reused, Reused::Yes);
        assert!(Rc::ptr_eq(&pane.0, &again.0));
        let (again, reused) = chats.take_or_make(&chat, || panic!("reuse chat"));
        assert_eq!(reused, Reused::Yes);
        assert!(Rc::ptr_eq(&page.0, &again.0));
    }
}
