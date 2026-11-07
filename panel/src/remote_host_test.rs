//! What a host's own test of the remote path builds on: a live tab over the test's provider, and what the test reads
//! back of it. Built only for tests (`test-support`); nothing here is a call a host makes.

use super::*;
use std::sync::Arc;

#[cfg(any(test, feature = "test-support"))]
impl AgentPanelHandle {
    /// Opens a live Claude tab whose conversation runs over `provider` in `cwd`, and returns it with its row key.
    pub fn open_live_tab_for_host_test(
        &self,
        provider: Arc<dyn agent::AgentProvider + Send + Sync>,
        cwd: &Path,
    ) -> (TabId, eitri_core::tab_set::RowKey) {
        let conversation = agent::AgentConversation::create(provider, cwd, ProjectTrust::Untrusted)
            .expect("a conversation over the test's provider");
        let backend = AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        };
        let mut state = self.state.borrow_mut();
        let tab = state.tabs.open(AgentId::Claude);
        let held = state.tabs.get_mut(tab).expect("the tab just opened");
        held.set_backend(TabBackend::Live(backend));
        let key = held.row_key().clone();
        (tab, key)
    }

    /// Opens a tab of `agent` that holds no session, as a tab is before its first send, and returns it with its row key.
    pub fn open_unstarted_tab_for_host_test(&self, agent: AgentId) -> eitri_core::tab_set::RowKey {
        let mut state = self.state.borrow_mut();
        let tab = state.tabs.open(agent);
        state.tabs.get(tab).expect("the tab just opened").row_key().clone()
    }

    /// The tab's session mode.
    pub fn mode_for_host_test(&self, tab: TabId) -> SessionModeChoice {
        self.state.borrow().tabs.get(tab).expect("the tab is open").mode()
    }

    /// The tab's backend generation, its active turn and the last revision its projection folded.
    pub fn tab_progress_for_host_test(&self, tab: TabId) -> (u64, Option<String>, u64) {
        let state = self.state.borrow();
        let held = state.tabs.get(tab).expect("the tab is open");
        let live = held.live().map(|backend| {
            let projection = backend.projection();
            (projection.active_turn_id.clone(), projection.last_revision)
        });
        let (turn, revision) = live.unwrap_or((None, 0));
        (held.backend_generation(), turn, revision)
    }

    /// How many remote observers follow this panel's tabs.
    pub fn observer_count_for_host_test(&self) -> usize {
        self.state.borrow().tabs.observer_count()
    }
}
