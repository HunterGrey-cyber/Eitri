use std::collections::BTreeMap;

use eitri_core::layout::ModuleId;

pub(crate) trait Pane {
    fn close(&self);
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Reused {
    Yes,
    No,
}

pub(crate) struct Terminals<P> {
    by_id: BTreeMap<ModuleId, P>,
    idle: BTreeMap<ModuleId, P>,
}

impl<P: Pane + Clone> Terminals<P> {
    pub(crate) fn new() -> Self {
        Self {
            by_id: BTreeMap::new(),
            idle: BTreeMap::new(),
        }
    }

    pub(crate) fn get(&self, id: &ModuleId) -> Option<&P> {
        self.by_id.get(id)
    }

    /// An idle pane recorded under `id` itself is reused; any other id gets `make()`'s.
    pub(crate) fn take_or_make(&mut self, id: &ModuleId, make: impl FnOnce() -> P) -> (P, Reused) {
        if let Some(pane) = self.by_id.get(id) {
            return (pane.clone(), Reused::Yes);
        }
        let (pane, reused) = match self.idle.remove(id) {
            Some(pane) => (pane, Reused::Yes),
            None => (make(), Reused::No),
        };
        self.by_id.insert(id.clone(), pane.clone());
        (pane, reused)
    }

    pub(crate) fn retire(&mut self, id: &ModuleId) -> bool {
        let Some(pane) = self.by_id.remove(id) else {
            return false;
        };
        pane.close();
        self.idle.insert(id.clone(), pane);
        true
    }

    pub(crate) fn live(&self) -> impl Iterator<Item = (&ModuleId, &P)> {
        self.by_id.iter()
    }

    pub(crate) fn each(&self) -> impl Iterator<Item = &P> {
        self.by_id.values().chain(self.idle.values())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::exit_is_current;
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

    impl Pane for FakePane {
        fn close(&self) {
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
