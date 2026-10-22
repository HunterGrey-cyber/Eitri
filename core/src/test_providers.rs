//! Test doubles shared by `agent_backend`'s and `tab_set`'s tests: a provider that queues whatever
//! events a test wants and records every resolution it is handed. Moved out of `agent_backend`'s
//! test module unchanged when the tab set needed the same double.

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
    /// While on, every `interrupt_turn` fails as a broken transport would: a fatal error, the session gone.
    failing_interrupts: AtomicBool,
    refusing: AtomicBool,
    /// While enabled, refuse every `resolve_permission` with a provider error.
    /// This exercises `AgentBackend::approve_pending`'s failed-allow path, which keeps a card.
    refusing_resolutions: AtomicBool,
    interrupt_capable: bool,
    /// Every `close_session` that reached the provider: what a backend's shutdown does.
    closes: AtomicUsize,
    /// Advertises `cli_auto_mode`: its sessions run the CLI's own auto mode, and it accepts a
    /// deferral -- which, like the real provider, it refuses without this.
    cli_auto: bool,
    /// Every answer that reached the provider, with the decision itself (`resolved` keeps only
    /// whether it allowed, which cannot tell a deferral from a denial).
    decisions: std::sync::Mutex<Vec<(String, agent::PermissionDecision)>>,
    /// Events to queue as the next accepted resolution is recorded, and to wait out until
    /// ingestion has folded them: what background ingestion does while a batch is being answered.
    after_resolution: std::sync::Mutex<Option<Vec<AgentDomainEvent>>>,
    /// How many times ingestion has asked this provider for events.
    pumps: AtomicUsize,
}

impl RecordingProvider {
    pub fn queue(&self, event: AgentDomainEvent) {
        self.queued.lock().unwrap().push(event);
    }
    /// Queues every event in `events`, in order, under ONE lock acquisition -- unlike calling
    /// [`Self::queue`] once per event, which races the ingestion thread's own `pump` (also one lock
    /// acquisition, taking whatever is queued at that instant). A test that pushes a "prefix" event
    /// (one whose fold satisfies some condition the test polls for) followed by a long filler run
    /// meant to overflow `UI_EVENT_QUEUE_CAPACITY` in the SAME batch cannot rely on `queue` for
    /// that: the ingestion thread can drain and fold the prefix alone, the instant after it is
    /// pushed and long before the filler loop finishes pushing the rest, so a test's `until` on the
    /// prefix's own effect can succeed before the batch has actually overflowed. Measured 11/60
    /// failures of `tab_set::tests::a_resync` at `--test-threads=8` under load with `queue`;
    /// 0/100 with every such prefix+filler sequence switched to this instead, because `pump` (a
    /// single `mem::take`) then either sees none of the batch or all of it, and `IngestState::fold`
    /// runs the whole batch under one lock hold -- so nothing outside can observe the prefix's
    /// effect until the overflow later in the same batch has already happened too.
    pub fn queue_all(&self, events: Vec<AgentDomainEvent>) {
        self.queued.lock().unwrap().extend(events);
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
    /// A provider whose sessions run the CLI's own auto mode under the gate
    /// (`ProviderCapabilities::cli_auto_mode`), so the host may defer to it.
    pub fn cli_auto() -> Self {
        RecordingProvider {
            cli_auto: true,
            ..Default::default()
        }
    }
    /// Arms one-shot: when the next resolution is accepted, `events` are queued and the call
    /// returns only once ingestion has folded them into the projection, so whoever answers next
    /// reads the changed projection. Without it a test cannot make a fold land between two answers
    /// of one batch deterministically.
    pub fn fold_after_next_resolution(&self, events: Vec<AgentDomainEvent>) {
        *self.after_resolution.lock().unwrap() = Some(events);
    }
    /// Every answer that reached the provider, in order, with its decision.
    pub fn decisions(&self) -> Vec<(String, agent::PermissionDecision)> {
        self.decisions.lock().unwrap().clone()
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
    /// While on, every interrupt fails as a dead session's would (fatal, not a complaint about timing).
    pub fn fail_interrupts(&self, on: bool) {
        self.failing_interrupts.store(on, Ordering::SeqCst);
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
            cli_auto_mode: self.cli_auto,
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
        if self.failing_interrupts.load(Ordering::SeqCst) {
            return Err(agent::ProviderError::Transport(
                "the scripted agent's pipe is closed".into(),
            ));
        }
        Ok(())
    }
    fn resolve_permission(&self, request: agent::ResolvePermissionRequest) -> Result<(), agent::ProviderError> {
        if self.refusing_resolutions.load(Ordering::SeqCst) {
            return Err(agent::ProviderError::Provider {
                code: agent::ProviderErrorCode::PermissionAlreadyResolved,
                message: "resolutions refused by the test".into(),
            });
        }
        if request.decision.defers() && !self.cli_auto {
            return Err(agent::ProviderError::UnsupportedCapability("permission_defer"));
        }
        self.decisions
            .lock()
            .unwrap()
            .push((request.permission_id.clone(), request.decision.clone()));
        self.resolved
            .lock()
            .unwrap()
            .push((request.permission_id, request.decision.allows()));
        if let Some(events) = self.after_resolution.lock().unwrap().take() {
            self.queue_all(events);
            // The ingestion thread folds what a `pump` returned before it calls `pump` again, so
            // two further calls mean the queued events have been taken and folded.
            let until = self.pumps.load(Ordering::SeqCst) + 2;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while self.pumps.load(Ordering::SeqCst) < until && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        Ok(())
    }
    fn close_session(&self, _request: agent::CloseSessionRequest) -> Result<(), agent::ProviderError> {
        self.closes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn pump(&self) -> Vec<AgentDomainEvent> {
        self.pumps.fetch_add(1, Ordering::SeqCst);
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

/// An external agent's provider (`agent::ExternalProvider`) over a [`RecordingProvider`]: it records
/// every start and the policy of every turn, and its shutdown can be held on a gate, for a test that
/// needs a backend whose shutdown has not returned yet. Its Claude-shaped session and turn doors are
/// closed, as a real external provider's are.
pub struct ExternalRecordingProvider {
    pub inner: RecordingProvider,
    name: &'static str,
    /// What `resumes` answers: an ACP agent's `loadSession`.
    resumes: bool,
    starts: std::sync::Mutex<Vec<agent::ExternalStart>>,
    policies: std::sync::Mutex<Vec<agent::TurnPolicy>>,
    /// While set, `close_session` waits for one message (or the sender's drop) before it closes.
    close_gate: std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    /// How many `stop_background` calls it took.
    stops: std::sync::Mutex<u32>,
    /// While set, the next `stop_background` is refused with this code.
    stop_refusal: std::sync::Mutex<Option<agent::ProviderErrorCode>>,
    /// Advertises `steer` and takes messages into the running turn, as Codex does.
    steering: bool,
    /// Every message it was sent into a running turn, in order.
    steers: std::sync::Mutex<Vec<agent::SteerTurnRequest>>,
    /// While set, every `steer_turn` is refused with this code (nothing sent).
    steer_refusal: std::sync::Mutex<Option<agent::ProviderErrorCode>>,
}

impl ExternalRecordingProvider {
    /// A provider that names itself `codex`, as Codex's does.
    pub fn codex() -> Self {
        Self::named("codex", true)
    }

    /// A provider that names itself `name` (`acp:<registered name>` for an ACP agent's) and answers
    /// `resumes` with `resumes`.
    pub fn named(name: &'static str, resumes: bool) -> Self {
        ExternalRecordingProvider {
            inner: RecordingProvider::interruptible(),
            name,
            resumes,
            starts: std::sync::Mutex::new(Vec::new()),
            policies: std::sync::Mutex::new(Vec::new()),
            close_gate: std::sync::Mutex::new(None),
            stops: std::sync::Mutex::new(0),
            stop_refusal: std::sync::Mutex::new(None),
            steering: false,
            steers: std::sync::Mutex::new(Vec::new()),
            steer_refusal: std::sync::Mutex::new(None),
        }
    }

    /// A Codex provider that takes messages into the running turn (`steer`).
    pub fn steering_codex() -> Self {
        ExternalRecordingProvider {
            steering: true,
            ..Self::codex()
        }
    }

    /// Every message sent into a running turn, in order.
    pub fn steers(&self) -> Vec<agent::SteerTurnRequest> {
        self.steers.lock().unwrap().clone()
    }

    /// Refuses every later `steer_turn` with `code` (`None` takes them again).
    pub fn refuse_steers(&self, code: Option<agent::ProviderErrorCode>) {
        *self.steer_refusal.lock().unwrap() = code;
    }

    /// How many stops of the background it took.
    pub fn stops(&self) -> u32 {
        *self.stops.lock().unwrap()
    }

    /// Refuses the next `stop_background` with `code`, as Codex does when a turn started or the
    /// commands ended meanwhile.
    pub fn refuse_next_stop(&self, code: agent::ProviderErrorCode) {
        *self.stop_refusal.lock().unwrap() = Some(code);
    }

    /// Every start it was handed, in order.
    pub fn starts(&self) -> Vec<agent::ExternalStart> {
        self.starts.lock().unwrap().clone()
    }

    /// The policy of every turn it was sent, in order.
    pub fn policies(&self) -> Vec<agent::TurnPolicy> {
        self.policies.lock().unwrap().clone()
    }

    /// Holds the next `close_session` until the returned sender sends or is dropped.
    pub fn hold_close(&self) -> std::sync::mpsc::Sender<()> {
        let (release, gate) = std::sync::mpsc::channel();
        *self.close_gate.lock().unwrap() = Some(gate);
        release
    }
}

impl agent::AgentProvider for ExternalRecordingProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            steer: self.steering,
            ..agent::AgentProvider::capabilities(&self.inner)
        }
    }
    fn steer_turn(&self, request: agent::SteerTurnRequest) -> Result<(), agent::ProviderError> {
        if !self.steering {
            return Err(agent::ProviderError::UnsupportedCapability("steer"));
        }
        if let Some(code) = *self.steer_refusal.lock().unwrap() {
            return Err(agent::ProviderError::Provider {
                code,
                message: "the scripted agent took no message".into(),
            });
        }
        self.steers.lock().unwrap().push(request);
        Ok(())
    }
    fn info(&self) -> ProviderInfo {
        agent::AgentProvider::info(&self.inner)
    }
    fn create_session(&self, _request: agent::CreateSessionRequest) -> Result<String, agent::ProviderError> {
        Err(agent::ProviderError::UnsupportedCapability("external"))
    }
    fn resume_session(&self, _request: agent::ResumeSessionRequest) -> Result<String, agent::ProviderError> {
        Err(agent::ProviderError::UnsupportedCapability("external"))
    }
    fn send_turn(&self, _request: agent::SendTurnRequest) -> Result<String, agent::ProviderError> {
        Err(agent::ProviderError::UnsupportedCapability("external"))
    }
    fn interrupt_turn(&self, request: agent::InterruptTurnRequest) -> Result<(), agent::ProviderError> {
        agent::AgentProvider::interrupt_turn(&self.inner, request)
    }
    fn resolve_permission(&self, request: agent::ResolvePermissionRequest) -> Result<(), agent::ProviderError> {
        agent::AgentProvider::resolve_permission(&self.inner, request)
    }
    fn close_session(&self, request: agent::CloseSessionRequest) -> Result<(), agent::ProviderError> {
        let gate = self.close_gate.lock().unwrap().take();
        if let Some(gate) = gate {
            let _ = gate.recv();
        }
        agent::AgentProvider::close_session(&self.inner, request)
    }
    fn pump(&self) -> Vec<AgentDomainEvent> {
        agent::AgentProvider::pump(&self.inner)
    }
}

impl agent::ExternalProvider for ExternalRecordingProvider {
    fn name(&self) -> &'static str {
        self.name
    }
    fn resumes(&self) -> bool {
        self.resumes
    }
    fn start(&self, req: agent::ExternalStart) -> Result<String, agent::ProviderError> {
        match &req.resume {
            Some(id) => self.inner.queue(AgentDomainEvent::ResumeOutcome {
                requested_provider_session_id: id.clone(),
                status: agent::ResumeStatus::Attached,
                attached_provider_session_id: Some(id.clone()),
                forked: false,
                detail: None,
            }),
            None => self.inner.queue(AgentDomainEvent::SessionOpened {
                session_id: "ext-session".into(),
                provider_session_id: "thread-1".into(),
                model: "gpt-test".into(),
                cwd: req.cwd.clone(),
            }),
        }
        self.starts.lock().unwrap().push(req);
        Ok("ext-session".into())
    }
    fn send_turn_with(
        &self,
        request: agent::SendTurnRequest,
        policy: agent::TurnPolicy,
    ) -> Result<String, agent::ProviderError> {
        self.policies.lock().unwrap().push(policy);
        agent::AgentProvider::send_turn(&self.inner, request)
    }
    /// As Codex: a session it stopped itself can be continued.
    fn stopped_resumable(&self) -> bool {
        self.stops() > 0
    }
    /// What Codex does once the stop is taken: the count drops, and the session ends once its child is
    /// gone.
    fn stop_background(&self, _session_id: &str, _shown: u32) -> Result<(), agent::ProviderError> {
        if let Some(code) = self.stop_refusal.lock().unwrap().take() {
            return Err(agent::ProviderError::Provider {
                code,
                message: "refused by the scripted provider".into(),
            });
        }
        *self.stops.lock().unwrap() += 1;
        self.inner.queue(AgentDomainEvent::BackgroundCalls { running: 0 });
        self.inner.queue(AgentDomainEvent::SessionUnavailable {
            reason: agent::providers::codex::provider::STOPPED_BACKGROUND.to_string(),
        });
        Ok(())
    }
}

/// A trust plan with nothing in it, for tests that start an external tab and do not look at trust.
pub fn empty_trust_plan() -> agent::TrustPlan {
    agent::TrustPlan {
        dirs: Vec::new(),
        skills_off: true,
        fingerprint: String::new(),
        project_only: Vec::new(),
    }
}
