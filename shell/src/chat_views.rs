//! Every extra chat view's page (`agent#2` to `agent#9`), keyed by the module id it was made for.
//!
//! A page is built once for one view and answers for that view only: its `eitriAgent` handler was given
//! the view when the page was built (`agent_panel::build_chat_page`), and nothing can change that later.
//! So a page that leaves the layout waits here, idle, under the same id, and only a new view of that same
//! number takes it back; any other number gets a page of its own. Moving a page to another id would make
//! its answers count for a view it was never built for.
//!
//! Generic over the page so the pool's rules are tested without a display: the window keeps
//! `(WebView, Rc<PanelPage>)` here, the tests a plain value.

use std::collections::BTreeMap;

use eitri_core::layout::ModuleId;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Reused {
    Yes,
    No,
}

/// Every extra chat view's page, live or idle, keyed by the id it was made for and never re-keyed.
pub(crate) struct ChatViews<P> {
    live: BTreeMap<ModuleId, P>,
    idle: BTreeMap<ModuleId, P>,
}

impl<P: Clone> ChatViews<P> {
    pub(crate) fn new() -> Self {
        Self {
            live: BTreeMap::new(),
            idle: BTreeMap::new(),
        }
    }

    /// The live page of `id`.
    pub(crate) fn get(&self, id: &ModuleId) -> Option<&P> {
        self.live.get(id)
    }

    /// The live page of `id`, else the idle one recorded under `id` itself, else `make()`'s.
    pub(crate) fn take_or_make(&mut self, id: &ModuleId, make: impl FnOnce() -> P) -> (P, Reused) {
        if let Some(page) = self.live.get(id) {
            return (page.clone(), Reused::Yes);
        }
        let (page, reused) = match self.idle.remove(id) {
            Some(page) => (page, Reused::Yes),
            None => (make(), Reused::No),
        };
        self.live.insert(id.clone(), page.clone());
        (page, reused)
    }

    /// `id`'s page leaves the live set and waits, idle, under `id`. `false` when `id` has no live page.
    pub(crate) fn retire(&mut self, id: &ModuleId) -> bool {
        let Some(page) = self.live.remove(id) else {
            return false;
        };
        self.idle.insert(id.clone(), page);
        true
    }

    /// The live pages, by id. The window reaches a page by its id; what reads the pool whole is the
    /// grid probe, which is built only with its feature.
    #[cfg_attr(not(any(test, feature = "probe")), allow(dead_code))]
    pub(crate) fn live(&self) -> impl Iterator<Item = (&ModuleId, &P)> {
        self.live.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::layout::ModuleKind;

    /// A page that says which id it was made for, and which page it is among those made for that id.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Page {
        made_for: u8,
        serial: u32,
    }

    fn agent(n: u8) -> ModuleId {
        ModuleId::instance(ModuleKind::Agent, n).unwrap()
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
