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
    task_control: bool,
    stopped_tasks: std::sync::Mutex<Vec<String>>,
    /// Every `set_model` and `apply_effort` that reached the provider, as `model x` or `effort y`.
    settings_calls: std::sync::Mutex<Vec<String>>,
    /// Every ranged read of a task's output that reached the provider.
    output_ranges: std::sync::Mutex<Vec<(u64, u32)>>,
    queued: std::sync::Mutex<Vec<AgentDomainEvent>>,
    resolved: std::sync::Mutex<Vec<(String, bool)>>,
    /// Every turn text a send reached the provider with, accepted or refused.
    turns: std::sync::Mutex<Vec<String>>,
    interrupts: AtomicUsize,
    /// While on, every `interrupt_turn` fails as a broken transport would: a fatal error, the session gone.
    failing_interrupts: AtomicBool,
    refusing: AtomicBool,
    /// While on, every `resolve_permission` is refused as a provider error. Covers
    /// `AgentBackend::approve_pending`'s failed-allow path ("fail toward a card").
    refusing_resolutions: AtomicBool,
    holding_permission_replies: AtomicBool,
    held_permission_replies: std::sync::Mutex<Vec<std::sync::mpsc::Sender<agent::InteractionReply>>>,
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
    held_pump: std::sync::Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
    /// Advertises `steer` and takes messages into the running turn.
    steering: bool,
    /// Every message it was sent into a running turn, in order.
    steers: std::sync::Mutex<Vec<agent::SteerTurnRequest>>,
    /// Every answer to a question that reached it, in order, taken or refused.
    interactions: std::sync::Mutex<Vec<agent::ResolveInteractionRequest>>,
    /// While set, every `resolve_interaction` is refused with this code (what the sidecar does with a
    /// spent id or an answer that breaks a rule of the set).
    refusing_interactions: std::sync::Mutex<Option<agent::ProviderErrorCode>>,
    /// While set, answers to questions get no reply until [`Self::reply_to_held_interactions`].
    holding_interaction_replies: std::sync::atomic::AtomicBool,
    held_interaction_replies: std::sync::Mutex<Vec<std::sync::mpsc::Sender<agent::InteractionReply>>>,
    turn_stamps: std::sync::Mutex<Vec<agent::TurnStamp>>,
    steer_stamps: std::sync::Mutex<Vec<agent::TurnStamp>>,
    /// Advertises `client_tools` and takes answers to client tool calls.
    client_tools: bool,
    /// Every answer to a client tool call that reached it, in order.
    client_answers: std::sync::Mutex<Vec<agent::ClientToolAnswer>>,
    /// While set, every answer to a client tool call is recorded and refused with this code.
    refusing_client_answers: std::sync::Mutex<Option<agent::ProviderErrorCode>>,
    /// What a read of the open calls replies, or `Err` when none is set: a sidecar that is gone.
    client_state: std::sync::Mutex<Option<agent::ClientToolCallState>>,
    /// While on, a read of the open calls gets no reply until the test gives or drops it.
    holding_client_reads: AtomicBool,
    held_client_reads: std::sync::Mutex<Vec<ClientReadReply>>,
    /// While set, `interrupt_turn` and `send_turn` wait on it before they do anything ([`Self::hold_calls`]).
    held_calls: std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    /// How many calls have begun to wait on the hold.
    calls_waiting: AtomicUsize,
}

type ClientReadReply = std::sync::mpsc::Sender<Result<agent::ClientToolCallState, agent::ProviderError>>;

impl RecordingProvider {
    pub fn hold_permission_replies(&self) {
        self.holding_permission_replies.store(true, Ordering::SeqCst);
    }

    /// Lets later permission calls reply normally while existing held replies stay pending.
    pub fn resume_permission_replies(&self) {
        self.holding_permission_replies.store(false, Ordering::SeqCst);
    }

    pub fn reply_to_held_permissions(&self, reply: agent::InteractionReply) {
        let mut held = self.held_permission_replies.lock().unwrap();
        assert_eq!(held.len(), 1);
        held.pop().unwrap().send(reply).unwrap();
    }

    /// Replies to the held answer at `index` among those still held, oldest first.
    pub fn reply_to_held_permission(&self, index: usize, reply: agent::InteractionReply) {
        let sender = self.held_permission_replies.lock().unwrap().remove(index);
        sender.send(reply).unwrap();
    }

    pub fn held_permissions(&self) -> usize {
        self.held_permission_replies.lock().unwrap().len()
    }

    /// A provider that serves tools the host declares, as a Claude sidecar of protocol 3.23 does.
    pub fn client_tooling() -> Self {
        Self {
            client_tools: true,
            ..Default::default()
        }
    }
    /// A provider that serves declared tools on a session running the CLI's own auto mode.
    pub fn client_tooling_cli_auto() -> Self {
        Self {
            client_tools: true,
            cli_auto: true,
            ..Default::default()
        }
    }
    /// Every answer to a client tool call, in order.
    pub fn client_answers(&self) -> Vec<agent::ClientToolAnswer> {
        self.client_answers.lock().unwrap().clone()
    }
    /// While `code` is set, every later answer to a client tool call is refused with it.
    pub fn refuse_client_answers(&self, code: Option<agent::ProviderErrorCode>) {
        *self.refusing_client_answers.lock().unwrap() = code;
    }
    /// What every later read of the open calls replies.
    pub fn serve_client_state(&self, state: agent::ClientToolCallState) {
        *self.client_state.lock().unwrap() = Some(state);
    }
    /// Leaves the reply to every later read of the open calls out: a sidecar that is slow to say.
    pub fn hold_client_reads(&self, on: bool) {
        self.holding_client_reads.store(on, Ordering::SeqCst);
    }
    /// Replies to every read held so far, with what [`Self::serve_client_state`] set.
    pub fn reply_to_held_client_reads(&self) {
        for reply in self.held_client_reads.lock().unwrap().drain(..) {
            let _ = reply.send(self.scripted_client_state());
        }
    }
    /// Every read held so far goes unanswered for good, as one does when its session closes.
    pub fn drop_held_client_reads(&self) {
        self.held_client_reads.lock().unwrap().clear();
    }
    fn scripted_client_state(&self) -> Result<agent::ClientToolCallState, agent::ProviderError> {
        self.client_state
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| agent::ProviderError::Transport("no state was scripted".into()))
    }
    /// What `set_model` and `apply_effort` were asked, in order.
    pub fn settings_calls(&self) -> Vec<String> {
        self.settings_calls.lock().unwrap().clone()
    }

    /// The ranged reads of a task's output, in order.
    pub fn output_ranges(&self) -> Vec<(u64, u32)> {
        self.output_ranges.lock().unwrap().clone()
    }

    pub fn task_controlling() -> Self {
        Self {
            task_control: true,
            ..Default::default()
        }
    }
    pub fn stopped_tasks(&self) -> Vec<String> {
        self.stopped_tasks.lock().unwrap().clone()
    }
    /// A subsequent pump starts only after ingestion folded the previous batch.
    pub fn pump_count(&self) -> usize {
        self.pumps.load(Ordering::SeqCst)
    }

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
    /// failures of the `a_resync…` tests of `tab_set::tests` at `--test-threads=8` under load with `queue`;
    /// 0/100 with every such prefix+filler sequence switched to this instead, because `pump` (a
    /// single `mem::take`) then either sees none of the batch or all of it, and `IngestState::fold`
    /// runs the whole batch under one lock hold -- so nothing outside can observe the prefix's
    /// effect until the overflow later in the same batch has already happened too.
    pub fn queue_all(&self, events: Vec<AgentDomainEvent>) {
        self.queued.lock().unwrap().extend(events);
    }

    /// Holds the next ingestion batch until the returned sender sends or is dropped; the receiver
    /// announces when ingestion is inside the provider's pump with its folding gate held.
    pub fn hold_next_pump(&self) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (inside, entered) = std::sync::mpsc::channel();
        let (release, held) = std::sync::mpsc::channel();
        *self.held_pump.lock().unwrap() = Some((inside, held));
        (entered, release)
    }
    /// Waits until ingestion has folded every event already queued, including a batch in flight.
    pub fn wait_until_folded(&self) {
        let target = self.pumps.load(Ordering::SeqCst) + 2;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while self.pumps.load(Ordering::SeqCst) < target {
            assert!(
                std::time::Instant::now() < deadline,
                "ingestion did not cross the queued batch"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    pub fn resolutions(&self) -> Vec<(String, bool)> {
        self.resolved.lock().unwrap().clone()
    }
    /// From now on every `interrupt_turn` and `send_turn` waits before it does anything, until the returned sender
    /// sends or is dropped: a provider slow to answer. Calls wait one at a time.
    pub fn hold_calls(&self) -> std::sync::mpsc::Sender<()> {
        let (release, held) = std::sync::mpsc::channel();
        *self.held_calls.lock().unwrap() = Some(held);
        release
    }
    /// How many calls have begun to wait on [`Self::hold_calls`]' hold.
    pub fn calls_waiting(&self) -> usize {
        self.calls_waiting.load(Ordering::SeqCst)
    }
    fn wait_if_held(&self) {
        let held = self.held_calls.lock().unwrap();
        if let Some(held) = held.as_ref() {
            self.calls_waiting.fetch_add(1, Ordering::SeqCst);
            let _ = held.recv();
        }
    }
    /// A provider that advertises `interrupt` (read by `AgentConversation::create`, so choose it
    /// before creating the conversation).
    pub fn interruptible() -> Self {
        RecordingProvider {
            interrupt_capable: true,
            ..Default::default()
        }
    }
    /// A provider that takes messages into the running turn, as the Claude sidecar does.
    pub fn steering() -> Self {
        RecordingProvider {
            steering: true,
            interrupt_capable: true,
            ..Default::default()
        }
    }
    /// Every message sent into a running turn, in order.
    pub fn steers(&self) -> Vec<agent::SteerTurnRequest> {
        self.steers.lock().unwrap().clone()
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
    pub fn turn_stamps(&self) -> Vec<agent::TurnStamp> {
        self.turn_stamps.lock().unwrap().clone()
    }
    pub fn steer_stamps(&self) -> Vec<agent::TurnStamp> {
        self.steer_stamps.lock().unwrap().clone()
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
    /// Every answer to a question that reached the provider, in order.
    pub fn interactions(&self) -> Vec<agent::ResolveInteractionRequest> {
        self.interactions.lock().unwrap().clone()
    }
    /// Refuses every later `resolve_interaction` with `code`, or none again with `None`.
    pub fn refuse_interactions(&self, code: Option<agent::ProviderErrorCode>) {
        *self.refusing_interactions.lock().unwrap() = code;
    }
    /// Leaves the reply to every later answer out until [`Self::reply_to_held_interactions`]: an
    /// agent that is slow to take an answer.
    pub fn hold_interaction_replies(&self, on: bool) {
        self.holding_interaction_replies.store(on, Ordering::SeqCst);
    }
    /// Replies `reply` to every answer held so far.
    pub fn reply_to_held_interactions(&self, reply: impl Fn() -> agent::InteractionReply) {
        for tx in self.held_interaction_replies.lock().unwrap().drain(..) {
            let _ = tx.send(reply());
        }
    }
}

impl agent::AgentProvider for RecordingProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            interrupt: self.interrupt_capable,
            cli_auto_mode: self.cli_auto,
            steer: self.steering,
            task_control: self.task_control,
            task_output: self.task_control,
            direct_settings: self.task_control,
            task_output_range: self.task_control,
            client_tools: self.client_tools,
            ..ProviderCapabilities::default()
        }
    }
    fn answer_client_tool_call(&self, answer: agent::ClientToolAnswer) -> Result<(), agent::ProviderError> {
        if !self.client_tools {
            return Err(agent::ProviderError::UnsupportedCapability("client_tools"));
        }
        self.client_answers.lock().unwrap().push(answer);
        match *self.refusing_client_answers.lock().unwrap() {
            Some(code) => Err(agent::ProviderError::Provider {
                code,
                message: "answers refused by the test".into(),
            }),
            None => Ok(()),
        }
    }
    fn client_tool_call_state(
        &self,
        _session_id: &str,
    ) -> Result<agent::TaskReply<agent::ClientToolCallState>, agent::ProviderError> {
        if !self.client_tools {
            return Err(agent::ProviderError::UnsupportedCapability("client_tools"));
        }
        let (tx, rx) = std::sync::mpsc::channel();
        if self.holding_client_reads.load(Ordering::SeqCst) {
            self.held_client_reads.lock().unwrap().push(tx);
        } else {
            tx.send(self.scripted_client_state()).unwrap();
        }
        Ok(rx)
    }
    fn steer_turn(&self, request: agent::SteerTurnRequest) -> Result<(), agent::ProviderError> {
        if !self.steering {
            return Err(agent::ProviderError::UnsupportedCapability("steer"));
        }
        self.steers.lock().unwrap().push(request);
        Ok(())
    }
    fn send_turn_stamped(
        &self,
        request: agent::SendTurnRequest,
        stamp: agent::TurnStamp,
    ) -> Result<String, agent::ProviderError> {
        self.turn_stamps.lock().unwrap().push(stamp);
        self.send_turn(request)
    }
    fn steer_turn_stamped(
        &self,
        request: agent::SteerTurnRequest,
        stamp: agent::TurnStamp,
    ) -> Result<(), agent::ProviderError> {
        self.steer_stamps.lock().unwrap().push(stamp);
        self.steer_turn(request)
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
        self.wait_if_held();
        self.turns.lock().unwrap().push(request.text);
        if self.refusing.load(Ordering::SeqCst) {
            return Err(agent::ProviderError::Provider {
                code: agent::ProviderErrorCode::TurnAlreadyActive,
                message: "refused by the test".into(),
            });
        }
        Ok("turn-1".into())
    }
    fn stop_background(&self, session_id: &str, _shown: u32, tasks: &[String]) -> Result<(), agent::ProviderError> {
        if !self.task_control {
            return Err(agent::ProviderError::UnsupportedCapability("stop_background"));
        }
        for task in tasks {
            let _reply = self.stop_task(session_id, task)?;
        }
        Ok(())
    }
    fn stop_task(&self, _session_id: &str, task_id: &str) -> Result<agent::TaskReply<()>, agent::ProviderError> {
        if !self.task_control {
            return Err(agent::ProviderError::UnsupportedCapability("stop_task"));
        }
        self.stopped_tasks.lock().unwrap().push(task_id.to_owned());
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Ok(())).unwrap();
        Ok(rx)
    }
    fn task_output(
        &self,
        _session_id: &str,
        _task_id: &str,
    ) -> Result<agent::TaskReply<agent::TaskOutput>, agent::ProviderError> {
        if !self.task_control {
            return Err(agent::ProviderError::UnsupportedCapability("task_output"));
        }
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Ok(agent::TaskOutput {
            output: "<b>plain</b>\u{202e}".into(),
            total_bytes: 10000,
            truncated: true,
            offset: 9000,
        }))
        .unwrap();
        Ok(rx)
    }
    fn task_output_range(
        &self,
        _session_id: &str,
        _task_id: &str,
        offset: u64,
        max_bytes: u32,
    ) -> Result<agent::TaskReply<agent::TaskOutput>, agent::ProviderError> {
        self.output_ranges.lock().unwrap().push((offset, max_bytes));
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Ok(agent::TaskOutput {
            output: "the whole".into(),
            total_bytes: 10000,
            truncated: offset > 0,
            offset,
        }))
        .unwrap();
        Ok(rx)
    }
    fn list_models(&self, _session_id: &str) -> Result<agent::TaskReply<agent::ModelList>, agent::ProviderError> {
        if !self.task_control {
            return Err(agent::ProviderError::UnsupportedCapability("list_models"));
        }
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Ok(agent::ModelList {
            models: vec![agent::ModelOption {
                value: "sonnet".into(),
                resolved_model: "claude-sonnet-5-5".into(),
                display_name: "Sonnet".into(),
                supports_auto_mode: true,
                ..Default::default()
            }],
            applied: agent::AppliedSettings {
                model: "claude-opus-5-5".into(),
                effort: "medium".into(),
            },
            efforts: Vec::new(),
        }))
        .unwrap();
        Ok(rx)
    }
    fn set_model(
        &self,
        _session_id: &str,
        model: &str,
    ) -> Result<agent::TaskReply<agent::AppliedSettings>, agent::ProviderError> {
        if !self.task_control {
            return Err(agent::ProviderError::UnsupportedCapability("set_model"));
        }
        self.settings_calls.lock().unwrap().push(format!("model {model}"));
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Ok(agent::AppliedSettings {
            model: format!("resolved-{model}"),
            effort: "medium".into(),
        }))
        .unwrap();
        Ok(rx)
    }
    fn apply_effort(
        &self,
        _session_id: &str,
        effort: &str,
    ) -> Result<agent::TaskReply<agent::AppliedSettings>, agent::ProviderError> {
        if !self.task_control {
            return Err(agent::ProviderError::UnsupportedCapability("apply_effort"));
        }
        self.settings_calls.lock().unwrap().push(format!("effort {effort}"));
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Ok(agent::AppliedSettings {
            model: "claude-opus-5-5".into(),
            effort: effort.into(),
        }))
        .unwrap();
        Ok(rx)
    }
    fn interrupt_turn(&self, _request: agent::InterruptTurnRequest) -> Result<(), agent::ProviderError> {
        self.wait_if_held();
        self.interrupts.fetch_add(1, Ordering::SeqCst);
        if self.failing_interrupts.load(Ordering::SeqCst) {
            return Err(agent::ProviderError::Transport(
                "the scripted agent's pipe is closed".into(),
            ));
        }
        Ok(())
    }
    fn resolve_permission(&self, request: agent::ResolvePermissionRequest) -> Result<(), agent::ProviderError> {
        if self.holding_permission_replies.load(Ordering::SeqCst) {
            let (tx, send) = agent::InteractionSend::channel();
            self.held_permission_replies.lock().unwrap().push(tx);
            return send.wait(std::time::Duration::from_secs(5)).map(|_| ());
        }
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
    fn resolve_permission_async(
        &self,
        request: agent::ResolvePermissionRequest,
    ) -> Result<agent::InteractionSend, agent::ProviderError> {
        if self.holding_permission_replies.load(Ordering::SeqCst) {
            let (tx, send) = agent::InteractionSend::channel();
            self.held_permission_replies.lock().unwrap().push(tx);
            return Ok(send);
        }
        self.resolve_permission(request)?;
        Ok(agent::InteractionSend::ready(Ok(None)))
    }
    fn resolve_interaction(
        &self,
        request: agent::ResolveInteractionRequest,
    ) -> Result<agent::InteractionSend, agent::ProviderError> {
        self.interactions.lock().unwrap().push(request);
        if self.holding_interaction_replies.load(Ordering::SeqCst) {
            let (tx, send) = agent::InteractionSend::channel();
            self.held_interaction_replies.lock().unwrap().push(tx);
            return Ok(send);
        }
        // The agent's refusal comes in the reply, as a real sidecar's does.
        Ok(agent::InteractionSend::ready(
            match *self.refusing_interactions.lock().unwrap() {
                Some(code) => Err(agent::ProviderError::Provider {
                    code,
                    message: "answers refused by the test".into(),
                }),
                None => Ok(None),
            },
        ))
    }
    fn close_session(&self, _request: agent::CloseSessionRequest) -> Result<(), agent::ProviderError> {
        self.closes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn pump(&self) -> Vec<AgentDomainEvent> {
        self.pumps.fetch_add(1, Ordering::SeqCst);
        let held = self.held_pump.lock().unwrap().take();
        if let Some((inside, release)) = held {
            let _ = inside.send(());
            let _ = release.recv_timeout(std::time::Duration::from_secs(10));
        }
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
    /// What `editor_note` hands over next.
    editor_note: std::sync::Mutex<Option<String>>,
    /// The sessions the agent lists as its own, and how many times it was asked for them again.
    listed: std::sync::Mutex<Vec<agent::ListedSession>>,
    refreshes: std::sync::atomic::AtomicU32,
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
            editor_note: std::sync::Mutex::new(None),
            listed: std::sync::Mutex::new(Vec::new()),
            refreshes: std::sync::atomic::AtomicU32::new(0),
        }
    }

    /// Lists `sessions` as the agent's own.
    pub fn list_sessions(&self, sessions: Vec<agent::ListedSession>) {
        *self.listed.lock().unwrap() = sessions;
    }

    /// How many times the agent was asked for its list of sessions again.
    pub fn refreshes(&self) -> u32 {
        self.refreshes.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Has `editor_note` hand `note` over, as a provider that left the session's tool server out.
    pub fn owe_editor_note(&self, note: &str) {
        *self.editor_note.lock().unwrap() = Some(note.to_string());
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
    /// What Codex does once the stop is taken: the count drops, and the session ends once its child is
    /// gone.
    fn stop_background(&self, _session_id: &str, _shown: u32, _tasks: &[String]) -> Result<(), agent::ProviderError> {
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
    fn listed_sessions(&self) -> Vec<agent::ListedSession> {
        self.listed.lock().unwrap().clone()
    }
    fn refresh_sessions(&self) {
        self.refreshes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
    fn editor_note(&self) -> Option<String> {
        self.editor_note.lock().unwrap().take()
    }
    /// As Codex: a session it stopped itself can be continued.
    fn stopped_resumable(&self) -> bool {
        self.stops() > 0
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

/// A provider that moves with its chat as the real ones do, over a [`RecordingProvider`] both windows of a test
/// drive: its freeze hands over three pipe descriptors and a record naming it, and [`adopt_moving`] takes the same
/// double up again in the other window, so one recording sees what both windows did. Every step of a move it is
/// asked for is kept in [`MovingProvider::steps`].
#[cfg(target_os = "linux")]
pub struct MovingProvider {
    pub inner: std::sync::Arc<RecordingProvider>,
    id: u64,
    live: AtomicBool,
    refuse_freeze: std::sync::Mutex<Option<String>>,
    steps: std::sync::Mutex<Vec<&'static str>>,
    /// The position of the stream it says it pumped through.
    position: std::sync::Mutex<Option<u64>>,
}

#[cfg(target_os = "linux")]
static MOVING: std::sync::Mutex<Vec<(u64, std::sync::Weak<MovingProvider>)>> = std::sync::Mutex::new(Vec::new());

#[cfg(target_os = "linux")]
static NEXT_MOVING: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

#[cfg(target_os = "linux")]
impl MovingProvider {
    /// A double over `inner` that keeps a running turn across a move when `live`.
    pub fn new(inner: RecordingProvider, live: bool) -> std::sync::Arc<MovingProvider> {
        let id = NEXT_MOVING.fetch_add(1, Ordering::SeqCst);
        let provider = std::sync::Arc::new(MovingProvider {
            inner: std::sync::Arc::new(inner),
            id,
            live: AtomicBool::new(live),
            refuse_freeze: std::sync::Mutex::new(None),
            steps: std::sync::Mutex::new(Vec::new()),
            position: std::sync::Mutex::new(None),
        });
        MOVING.lock().unwrap().push((id, std::sync::Arc::downgrade(&provider)));
        provider
    }
    /// The steps of a move asked of it so far, in order: `freeze`, `resume`, `adopt`, `commit`, `forget`.
    pub fn steps(&self) -> Vec<&'static str> {
        self.steps.lock().unwrap().clone()
    }
    /// Whatever it pumps from now on is at `position` of its stream, the place a move's record reads on from.
    pub fn report_position(&self, position: u64) {
        *self.position.lock().unwrap() = Some(position);
    }
    /// The next freezes are refused with `why`, or go through again with `None`.
    pub fn refuse_freezes(&self, why: Option<&str>) {
        *self.refuse_freeze.lock().unwrap() = why.map(str::to_owned);
    }
    fn step(&self, step: &'static str) {
        self.steps.lock().unwrap().push(step);
    }
}

/// [`agent::chat_mover::adopt`] for a [`MovingProvider`]'s record: the double it names, taken up as the other
/// window's provider, its three descriptors checked and closed.
#[cfg(target_os = "linux")]
pub fn adopt_moving(
    kind: agent::chat_mover::MovedKind,
    record: &serde_json::Value,
    fds: Vec<std::os::fd::OwnedFd>,
) -> Result<agent::chat_mover::Adopted, String> {
    if kind != agent::chat_mover::MovedKind::Claude || fds.len() != 3 {
        return Err(format!(
            "not a moving double's record: {kind:?} with {} descriptors",
            fds.len()
        ));
    }
    let id = record["double"].as_u64().ok_or("the record names no double")?;
    let provider = MOVING
        .lock()
        .unwrap()
        .iter()
        .find(|(held, _)| *held == id)
        .and_then(|(_, provider)| provider.upgrade())
        .ok_or("the double the record names is gone")?;
    provider.step("adopt");
    Ok(agent::chat_mover::Adopted {
        agent: provider,
        external: None,
    })
}

#[cfg(target_os = "linux")]
impl agent::chat_mover::ChatMover for MovingProvider {
    fn live_move(&self) -> bool {
        self.live.load(Ordering::SeqCst)
    }
    fn freeze_for_chat_move(&self) -> Result<agent::chat_mover::FrozenProvider, String> {
        if let Some(why) = self.refuse_freeze.lock().unwrap().clone() {
            return Err(why);
        }
        self.step("freeze");
        let fds = (0..3)
            .map(|_| std::io::pipe().map(|(read, _)| read.into()))
            .collect::<std::io::Result<Vec<std::os::fd::OwnedFd>>>()
            .map_err(|e| e.to_string())?;
        Ok(agent::chat_mover::FrozenProvider {
            kind: agent::chat_mover::MovedKind::Claude,
            record: serde_json::json!({ "double": self.id }),
            fds,
        })
    }
    fn resume_after_chat_move(&self, frozen: agent::chat_mover::FrozenProvider) -> Result<(), String> {
        drop(frozen);
        self.step("resume");
        Ok(())
    }
    fn commit_chat_move(&self) -> std::io::Result<()> {
        self.step("commit");
        Ok(())
    }
    fn forget_chat_move(&self) {
        self.step("forget");
    }
}

#[cfg(target_os = "linux")]
impl agent::AgentProvider for MovingProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        self.inner.capabilities()
    }
    fn info(&self) -> ProviderInfo {
        self.inner.info()
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
    fn answer_client_tool_call(&self, answer: agent::ClientToolAnswer) -> Result<(), agent::ProviderError> {
        self.inner.answer_client_tool_call(answer)
    }
    fn client_tool_call_state(
        &self,
        session_id: &str,
    ) -> Result<agent::TaskReply<agent::ClientToolCallState>, agent::ProviderError> {
        self.inner.client_tool_call_state(session_id)
    }
    fn pump(&self) -> Vec<AgentDomainEvent> {
        self.inner.pump()
    }
    fn pumped_through(&self) -> Option<u64> {
        *self.position.lock().unwrap()
    }
    fn mover(&self) -> Option<&dyn agent::chat_mover::ChatMover> {
        Some(self)
    }
}
