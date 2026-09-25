//! Test doubles shared by `agent_backend`'s and `tab_set`'s tests: a provider that queues whatever
//! events a test wants and records every resolution it is handed. Moved out of `agent_backend`'s
//! test module unchanged when the tab set needed the same double (session tabs plan, Task 5).

use agent::{AgentDomainEvent, ProviderCapabilities, ProviderInfo};

/// A provider that queues whatever events a test wants and records every resolution it is
/// handed. The twin of `agent_backend`'s `RejectingProvider`, for the opposite question: that one exists to
/// make a send fail, this one to see what the host answered.
#[derive(Default)]
pub struct RecordingProvider {
    queued: std::sync::Mutex<Vec<AgentDomainEvent>>,
    resolved: std::sync::Mutex<Vec<(String, bool)>>,
}

impl RecordingProvider {
    pub fn queue(&self, event: AgentDomainEvent) {
        self.queued.lock().unwrap().push(event);
    }
    pub fn resolutions(&self) -> Vec<(String, bool)> {
        self.resolved.lock().unwrap().clone()
    }
}

impl agent::AgentProvider for RecordingProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::default()
    }
    fn info(&self) -> ProviderInfo {
        ProviderInfo::default()
    }
    fn create_session(&self, _request: agent::CreateSessionRequest) -> Result<String, agent::ProviderError> {
        Ok("fake-session".into())
    }
    fn resume_session(&self, _request: agent::ResumeSessionRequest) -> Result<String, agent::ProviderError> {
        Err(agent::ProviderError::UnsupportedCapability("resume"))
    }
    fn send_turn(&self, _request: agent::SendTurnRequest) -> Result<String, agent::ProviderError> {
        Ok("turn-1".into())
    }
    fn interrupt_turn(&self, _request: agent::InterruptTurnRequest) -> Result<(), agent::ProviderError> {
        Ok(())
    }
    fn resolve_permission(&self, request: agent::ResolvePermissionRequest) -> Result<(), agent::ProviderError> {
        self.resolved
            .lock()
            .unwrap()
            .push((request.permission_id, request.decision.allows()));
        Ok(())
    }
    fn close_session(&self, _request: agent::CloseSessionRequest) -> Result<(), agent::ProviderError> {
        Ok(())
    }
    fn pump(&self) -> Vec<AgentDomainEvent> {
        std::mem::take(&mut *self.queued.lock().unwrap())
    }
}

impl RecordingProvider {
    /// What the Agent SDK's system/init becomes: the event that names the Claude session.
    pub fn open_session(&self, provider_session_id: &str, cwd: &std::path::Path) {
        self.queue(AgentDomainEvent::SessionOpened {
            session_id: "fake-session".into(),
            provider_session_id: provider_session_id.into(),
            model: "m".into(),
            cwd: cwd.to_string_lossy().into_owned(),
        });
    }
}
