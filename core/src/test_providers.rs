//! Test doubles shared by `agent_backend`'s and `tab_set`'s tests: a provider that queues whatever
//! events a test wants and records every resolution it is handed. Moved out of `agent_backend`'s
//! test module unchanged when the tab set needed the same double (session tabs plan, Task 5).

use agent::{AgentDomainEvent, ProviderCapabilities, ProviderInfo};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// A provider that queues whatever events a test wants and records every resolution it is
/// handed. The twin of `agent_backend`'s `RejectingProvider`, for the opposite question: that one exists to
/// make a send fail, this one to see what the host answered.
#[derive(Default)]
pub struct RecordingProvider {
    queued: std::sync::Mutex<Vec<AgentDomainEvent>>,
    resolved: std::sync::Mutex<Vec<(String, bool)>>,
    /// Every turn text a send reached the provider with, accepted or refused.
    turns: std::sync::Mutex<Vec<String>>,
    interrupts: AtomicUsize,
    refusing: AtomicBool,
    /// While on, every `resolve_permission` is refused as a provider error. R07/S2, Task 2: covers
    /// `AgentBackend::approve_pending`'s failed-allow path ("fail toward a card").
    refusing_resolutions: AtomicBool,
    interrupt_capable: bool,
    /// Every `close_session` that reached the provider: what a backend's shutdown does.
    closes: AtomicUsize,
}

impl RecordingProvider {
    pub fn queue(&self, event: AgentDomainEvent) {
        self.queued.lock().unwrap().push(event);
    }
    pub fn resolutions(&self) -> Vec<(String, bool)> {
        self.resolved.lock().unwrap().clone()
    }
    /// A provider that advertises `interrupt` (read by `AgentConversation::create`, so choose it
    /// before creating the conversation).
    pub fn interruptible() -> Self {
        RecordingProvider {
            interrupt_capable: true,
            ..Default::default()
        }
    }
    /// How many times the session was closed -- non-zero once its backend was shut down.
    pub fn closes(&self) -> usize {
        self.closes.load(Ordering::SeqCst)
    }
    pub fn turns(&self) -> Vec<String> {
        self.turns.lock().unwrap().clone()
    }
    pub fn interrupts(&self) -> usize {
        self.interrupts.load(Ordering::SeqCst)
    }
    /// While on, every send is refused as a provider error (benign: the session stays).
    pub fn refuse_sends(&self, on: bool) {
        self.refusing.store(on, Ordering::SeqCst);
    }
    /// While on, every `resolve_permission` (an allow or a deny) is refused as a provider error.
    pub fn refuse_resolutions(&self, on: bool) {
        self.refusing_resolutions.store(on, Ordering::SeqCst);
    }
}

impl agent::AgentProvider for RecordingProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            interrupt: self.interrupt_capable,
            ..ProviderCapabilities::default()
        }
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
    fn send_turn(&self, request: agent::SendTurnRequest) -> Result<String, agent::ProviderError> {
        self.turns.lock().unwrap().push(request.text);
        if self.refusing.load(Ordering::SeqCst) {
            return Err(agent::ProviderError::Provider {
                code: agent::ProviderErrorCode::TurnAlreadyActive,
                message: "refused by the test".into(),
            });
        }
        Ok("turn-1".into())
    }
    fn interrupt_turn(&self, _request: agent::InterruptTurnRequest) -> Result<(), agent::ProviderError> {
        self.interrupts.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn resolve_permission(&self, request: agent::ResolvePermissionRequest) -> Result<(), agent::ProviderError> {
        if self.refusing_resolutions.load(Ordering::SeqCst) {
            return Err(agent::ProviderError::Provider {
                code: agent::ProviderErrorCode::PermissionAlreadyResolved,
                message: "resolutions refused by the test".into(),
            });
        }
        self.resolved
            .lock()
            .unwrap()
            .push((request.permission_id, request.decision.allows()));
        Ok(())
    }
    fn close_session(&self, _request: agent::CloseSessionRequest) -> Result<(), agent::ProviderError> {
        self.closes.fetch_add(1, Ordering::SeqCst);
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
