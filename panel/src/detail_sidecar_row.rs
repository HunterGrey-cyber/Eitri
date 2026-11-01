//! The detail popover's revision row says which sidecar a tab runs and where it came from: `sidecar 0.3.3
//! (update)` for a tab on a selected update, `sidecar 0.3.0 (bundled)` for the one Eitri ships, and a checkout's or
//! a named binary's own description for those.

#[cfg(test)]
mod tests {
    use super::super::detail::detail_rows;
    use agent::setting_sources::ProjectTrust;
    use agent::{
        AgentConversation, AgentDomainEvent, AgentProvider, ProviderCapabilities, ProviderInfo, BUNDLED_SIDECAR_VERSION,
    };
    use eitri_core::agent_backend::{AgentBackend, AgentId, BackendKind};
    use eitri_core::agent_bridge::SessionModeChoice;
    use eitri_core::tab_set::{TabBackend, TabSet};
    use eitri_core::test_providers::RecordingProvider;
    use std::sync::Arc;

    /// A recording provider that reports `info` as the sidecar's own handshake would.
    struct Informed {
        inner: RecordingProvider,
        info: ProviderInfo,
    }

    impl AgentProvider for Informed {
        fn capabilities(&self) -> ProviderCapabilities {
            self.inner.capabilities()
        }
        fn info(&self) -> ProviderInfo {
            self.info.clone()
        }
        fn create_session(&self, request: agent::CreateSessionRequest) -> Result<String, agent::ProviderError> {
            self.inner.create_session(request)
        }
        fn resume_session(&self, request: agent::ResumeSessionRequest) -> Result<String, agent::ProviderError> {
            self.inner.resume_session(request)
        }
        fn send_turn(&self, request: agent::SendTurnRequest) -> Result<String, agent::ProviderError> {
            self.inner.send_turn(request)
        }
        fn interrupt_turn(&self, request: agent::InterruptTurnRequest) -> Result<(), agent::ProviderError> {
            self.inner.interrupt_turn(request)
        }
        fn resolve_permission(&self, request: agent::ResolvePermissionRequest) -> Result<(), agent::ProviderError> {
            self.inner.resolve_permission(request)
        }
        fn close_session(&self, request: agent::CloseSessionRequest) -> Result<(), agent::ProviderError> {
            self.inner.close_session(request)
        }
        fn pump(&self) -> Vec<AgentDomainEvent> {
            self.inner.pump()
        }
    }

    const PACKAGED: &str = "packaged sidecar artifact: /usr/lib/eitri/verdandi-claude-sidecar";

    /// The row a tab on a sidecar reporting `version` (an update, when `update`) and built as `description` is given.
    fn revision_row(version: &str, update: bool, description: &str) -> String {
        let dir = agent::state_dirs::test_workspace_dir("panel-sidecar-revision-row");
        let provider = Arc::new(Informed {
            inner: RecordingProvider::default(),
            info: ProviderInfo {
                sidecar_version: version.to_string(),
                protocol_major: 3,
                protocol_minor: 24,
                build_description: Some(description.to_string()),
                sidecar_update: update.then(|| version.to_string()),
                ..ProviderInfo::default()
            },
        });
        let conversation = AgentConversation::create(provider, &dir, ProjectTrust::Untrusted).unwrap();
        let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Manual);
        let tab = set.active();
        set.get_mut(tab)
            .unwrap()
            .set_backend(TabBackend::Live(AgentBackend::Conversation {
                agent: AgentId::Claude,
                conversation: Box::new(conversation),
            }));
        let rows = detail_rows(
            set.active_tab(),
            BackendKind::Sidecar,
            None,
            &dir,
            None,
            "not checked yet",
            ProjectTrust::Untrusted,
            &[],
            None,
        );
        let value = rows
            .iter()
            .find(|row| row.label == "Verdandi revision")
            .unwrap_or_else(|| {
                panic!(
                    "no revision row in {:?}",
                    rows.iter().map(|r| &r.label).collect::<Vec<_>>()
                )
            })
            .value
            .clone();
        if let Some(retired) = set.take_all().into_iter().find_map(|tab| tab.retired) {
            retired.into_backend().shutdown();
        }
        value
    }

    #[test]
    fn a_tab_on_a_selected_update_reads_sidecar_version_update() {
        assert_eq!(revision_row("0.3.3", true, PACKAGED), "sidecar 0.3.3 (update)");
    }

    #[test]
    fn a_tab_on_the_bundled_sidecar_reads_sidecar_version_bundled() {
        assert_eq!(
            revision_row(BUNDLED_SIDECAR_VERSION, false, PACKAGED),
            format!("sidecar {BUNDLED_SIDECAR_VERSION} (bundled)")
        );
    }

    #[test]
    fn a_checkout_is_described_even_at_the_bundled_version() {
        let checkout = "Verdandi checkout: /src/verdandi @ 0123456 (via EITRI_VERDANDI_CHECKOUT)";
        assert_eq!(
            revision_row(BUNDLED_SIDECAR_VERSION, false, checkout),
            format!("sidecar {BUNDLED_SIDECAR_VERSION} ({checkout})")
        );
    }

    #[test]
    fn a_named_binary_is_described() {
        let named = "sidecar binary: /opt/sidecar (via EITRI_SIDECAR_BINARY)";
        assert_eq!(
            revision_row(BUNDLED_SIDECAR_VERSION, false, named),
            format!("sidecar {BUNDLED_SIDECAR_VERSION} ({named})")
        );
    }

    /// A move hands over the description and not how the sidecar was chosen.
    #[test]
    fn a_chat_that_moved_on_an_update_is_not_called_bundled() {
        let moved = "packaged sidecar artifact: /data/eitri/sidecar/updates/9.9.9/verdandi-claude-sidecar";
        assert_eq!(revision_row("9.9.9", false, moved), format!("sidecar 9.9.9 ({moved})"));
    }
}
