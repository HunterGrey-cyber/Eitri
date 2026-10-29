//! An `AgentConversation` over an external provider: the record and the lease carry the provider's
//! name, the Claude-only steps of a resume are skipped, and the plain turn door is closed. Driven by
//! a scripted provider, so nothing here starts a process or spends a model turn.

use agent::lease::SessionLease;
use agent::{
    AgentConversation, AgentDomainEvent, AgentProvider, CloseSessionRequest, ConversationError, CreateSessionRequest,
    DirTrust, ExternalProvider, ExternalStart, InterruptTurnRequest, ProviderCapabilities, ProviderError, ProviderInfo,
    ResolvePermissionRequest, ResumeSessionRequest, ResumeStatus, Reviewer, SendTurnRequest, TrustPlan, TurnPolicy,
};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex, Once};

/// Where a Claude transcript would be read from, set once for the whole binary before any test reads
/// the environment: a test that plants a transcript must not touch the developer's own `~/.claude`.
fn setup() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let dir = agent::state_dirs::redirect_state_to_a_test_root().join("claude-config");
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("CLAUDE_CONFIG_DIR", dir);
    });
}

fn workspace(label: &str) -> std::path::PathBuf {
    setup();
    agent::state_dirs::test_workspace_dir(label)
}

#[derive(Default)]
struct Script {
    events: VecDeque<AgentDomainEvent>,
    started: Vec<ExternalStart>,
    turns: Vec<(String, TurnPolicy)>,
    plain_calls: Vec<&'static str>,
    /// What a resume reads back as the session's past, handed over through `take_history`.
    history: Vec<AgentDomainEvent>,
}

/// A provider that answers like an external one: the plain session and turn calls refuse, `start`
/// opens the session it is told to, and the events it reports are the ones the test queued.
struct Scripted {
    name: &'static str,
    /// Answers the plain `resume_session` too, as the Claude provider does (the control run).
    claude_like: bool,
    script: Mutex<Script>,
}

impl Scripted {
    fn new(name: &'static str) -> Arc<Self> {
        Arc::new(Self {
            name,
            claude_like: false,
            script: Mutex::default(),
        })
    }

    fn claude_like() -> Arc<Self> {
        Arc::new(Self {
            name: "claude-control",
            claude_like: true,
            script: Mutex::default(),
        })
    }
}

impl AgentProvider for Scripted {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            resume: true,
            interrupt: true,
            ..Default::default()
        }
    }
    fn info(&self) -> ProviderInfo {
        ProviderInfo::default()
    }
    fn create_session(&self, _: CreateSessionRequest) -> Result<String, ProviderError> {
        self.script.lock().unwrap().plain_calls.push("create_session");
        Err(ProviderError::UnsupportedCapability("external"))
    }
    fn resume_session(&self, _: ResumeSessionRequest) -> Result<String, ProviderError> {
        self.script.lock().unwrap().plain_calls.push("resume_session");
        if self.claude_like {
            return Ok("control".to_string());
        }
        Err(ProviderError::UnsupportedCapability("external"))
    }
    fn send_turn(&self, _: SendTurnRequest) -> Result<String, ProviderError> {
        self.script.lock().unwrap().plain_calls.push("send_turn");
        Err(ProviderError::UnsupportedCapability("external"))
    }
    fn interrupt_turn(&self, _: InterruptTurnRequest) -> Result<(), ProviderError> {
        Ok(())
    }
    fn resolve_permission(&self, _: ResolvePermissionRequest) -> Result<(), ProviderError> {
        Ok(())
    }
    fn close_session(&self, _: CloseSessionRequest) -> Result<(), ProviderError> {
        Ok(())
    }
    fn pump(&self) -> Vec<AgentDomainEvent> {
        self.script.lock().unwrap().events.drain(..).collect()
    }
}

impl ExternalProvider for Scripted {
    fn name(&self) -> &'static str {
        self.name
    }
    fn resumes(&self) -> bool {
        true
    }
    fn start(&self, req: ExternalStart) -> Result<String, ProviderError> {
        let mut script = self.script.lock().unwrap();
        let session_id = "ext-session".to_string();
        match &req.resume {
            Some(id) => script.events.push_back(AgentDomainEvent::ResumeOutcome {
                requested_provider_session_id: id.clone(),
                status: ResumeStatus::Attached,
                attached_provider_session_id: Some(id.clone()),
                forked: false,
                detail: None,
            }),
            None => script.events.push_back(AgentDomainEvent::SessionOpened {
                session_id: session_id.clone(),
                provider_session_id: "thread-1".to_string(),
                model: "m".to_string(),
                cwd: req.cwd.clone(),
            }),
        }
        script.started.push(req);
        Ok(session_id)
    }
    fn send_turn_with(&self, req: SendTurnRequest, policy: TurnPolicy) -> Result<String, ProviderError> {
        self.script.lock().unwrap().turns.push((req.text, policy));
        Ok("turn-1".to_string())
    }
    fn take_history(&self) -> Vec<AgentDomainEvent> {
        std::mem::take(&mut self.script.lock().unwrap().history)
    }
}

fn start_for(cwd: &Path, resume: Option<&str>) -> ExternalStart {
    ExternalStart {
        cwd: cwd.to_string_lossy().to_string(),
        plan: TrustPlan {
            dirs: vec![(cwd.to_path_buf(), DirTrust::Untrusted)],
            skills_off: true,
            fingerprint: "fp".to_string(),
            project_only: Vec::new(),
        },
        policy: TurnPolicy {
            reviewer: Reviewer::Agent,
        },
        resume: resume.map(str::to_string),
    }
}

fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !ready() {
        assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
fn external_conversation_records_carry_the_provider_name() {
    let cwd = workspace("ext-record");
    let provider = Scripted::new("codex");
    let mut conversation =
        AgentConversation::create_external(provider.clone(), start_for(&cwd, None), &cwd).expect("the start succeeds");
    assert_eq!(conversation.provider_name(), "codex");
    let conversation_id = conversation.conversation_id().to_string();

    // The ingestion thread adopts the session when its `SessionOpened` arrives: lease, then record.
    wait_for("the record to be written", || {
        agent::resumable_sessions(&conversation_id)
            .iter()
            .any(|r| r.provider_session_id == "thread-1")
    });
    let sessions = agent::resumable_sessions(&conversation_id);
    let recorded = sessions.iter().find(|r| r.provider_session_id == "thread-1").unwrap();
    assert_eq!(recorded.provider, "codex");

    let canonical = cwd.canonicalize().unwrap();
    let canonical = canonical.to_string_lossy();
    assert!(
        SessionLease::is_held("codex", &canonical, "thread-1").unwrap(),
        "the lease is keyed by the provider's name"
    );
    assert!(
        !SessionLease::is_held("claude", &canonical, "thread-1").unwrap(),
        "and not by the Claude name"
    );

    // The start the provider saw names the canonical directory and carries the plan unchanged.
    let started = provider.script.lock().unwrap().started.clone();
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].cwd, canonical);
    assert_eq!(started[0].plan.fingerprint, "fp");
    conversation.shutdown();
}

#[test]
fn plain_send_turn_on_external_is_refused() {
    let cwd = workspace("ext-plain");
    let provider = Scripted::new("codex");
    let mut conversation =
        AgentConversation::create_external(provider.clone(), start_for(&cwd, None), &cwd).expect("the start succeeds");
    let refused = conversation.send_turn("hello").unwrap_err();
    assert!(matches!(refused, ConversationError::Unsupported), "{refused:?}");
    assert!(refused.is_benign());
    assert!(
        provider.script.lock().unwrap().plain_calls.is_empty(),
        "the provider is not even asked"
    );

    // The door that is open carries the policy.
    let policy = TurnPolicy {
        reviewer: Reviewer::User,
    };
    assert_eq!(conversation.send_turn_with("hello", policy).unwrap(), "turn-1");
    assert_eq!(
        provider.script.lock().unwrap().turns,
        vec![("hello".to_string(), policy)]
    );
    conversation.shutdown();
}

#[test]
fn send_turn_with_refuses_while_a_turn_is_active() {
    let cwd = workspace("ext-active");
    let provider = Scripted::new("codex");
    let mut conversation =
        AgentConversation::create_external(provider.clone(), start_for(&cwd, None), &cwd).expect("the start succeeds");
    provider
        .script
        .lock()
        .unwrap()
        .events
        .push_back(AgentDomainEvent::TurnStarted {
            turn_id: "turn-0".to_string(),
        });
    wait_for("the turn to be folded", || {
        conversation.projection().active_turn_id.is_some()
    });
    let policy = TurnPolicy {
        reviewer: Reviewer::Agent,
    };
    assert!(matches!(
        conversation.send_turn_with("again", policy),
        Err(ConversationError::TurnAlreadyActive)
    ));
    assert!(provider.script.lock().unwrap().turns.is_empty());
    conversation.shutdown();
}

#[test]
fn create_external_refuses_a_start_that_names_a_session() {
    let cwd = workspace("ext-create-resume");
    let provider = Scripted::new("codex");
    let refused = AgentConversation::create_external(provider.clone(), start_for(&cwd, Some("thread-9")), &cwd);
    assert!(matches!(refused, Err(ConversationError::Provider(_))));
    assert!(provider.script.lock().unwrap().started.is_empty());
}

#[test]
fn resume_external_does_not_read_a_claude_transcript() {
    let cwd = workspace("ext-resume");
    let canonical = cwd.canonicalize().unwrap();
    let canonical_str = canonical.to_string_lossy().to_string();
    let marker = "DISTINCTIVE-CLAUDE-REPLY-7731";

    // The same transcript under two session ids: one for the Claude control run, one for the
    // external resume under test.
    let transcript_dir = agent::transcript::claude_projects_dir()
        .unwrap()
        .join(agent::transcript::sanitize_cwd_for_claude_projects(&canonical_str));
    std::fs::create_dir_all(&transcript_dir).unwrap();
    let line = format!(
        r#"{{"type":"assistant","uuid":"a-1","version":"2.1.272","message":{{"role":"assistant","content":[{{"type":"text","text":"{marker}"}}]}}}}"#
    );
    for id in ["control-session", "thread-resumed"] {
        std::fs::write(transcript_dir.join(format!("{id}.jsonl")), format!("{line}\n")).unwrap();
    }
    let said = |c: &AgentConversation| c.projection().transcript.iter().any(|m| m.text.contains(marker));

    // Control: the Claude path over this very provider reads the planted transcript, so the
    // assertion below can fail.
    let control = Scripted::claude_like();
    control
        .script
        .lock()
        .unwrap()
        .events
        .push_back(AgentDomainEvent::ResumeOutcome {
            requested_provider_session_id: "control-session".to_string(),
            status: ResumeStatus::Attached,
            attached_provider_session_id: Some("control-session".to_string()),
            forked: false,
            detail: None,
        });
    let mut claude_path = AgentConversation::resume(
        control,
        &canonical,
        "control-session",
        agent::setting_sources::ProjectTrust::Untrusted,
    )
    .expect("the control resume is attached");
    assert!(said(&claude_path), "the control run must see the planted transcript");
    claude_path.shutdown();

    let provider = Scripted::new("codex");
    let lease = SessionLease::try_acquire("codex", &canonical_str, "thread-resumed").unwrap();
    let mut conversation = AgentConversation::resume_external(
        provider.clone(),
        start_for(&canonical, Some("thread-resumed")),
        &canonical,
        lease,
    )
    .expect("the resume is attached");
    assert!(!said(&conversation), "the Claude transcript was read");
    assert!(conversation.projection().transcript.is_empty());
    assert_eq!(conversation.provider_name(), "codex");

    // The record is the external provider's.
    let sessions = agent::resumable_sessions(conversation.conversation_id());
    let recorded = sessions
        .iter()
        .find(|r| r.provider_session_id == "thread-resumed")
        .unwrap();
    assert_eq!(recorded.provider, "codex");
    assert_eq!(
        provider.script.lock().unwrap().started[0].resume.as_deref(),
        Some("thread-resumed")
    );
    conversation.shutdown();
}

/// A resumed external session's past turns are its history, not turns happening now: they are folded into
/// the projection before ingestion starts, as a Claude resume's transcript is, and never delivered as live
/// events -- which would have the host take each past turn for one that just ran (turn review's snapshots
/// and overlaps, the queue's flush, attention).
#[test]
fn resume_external_seeds_the_history_and_delivers_no_past_turn() {
    let cwd = workspace("ext-resume-history");
    let canonical = cwd.canonicalize().unwrap();
    let canonical_str = canonical.to_string_lossy().to_string();
    let provider = Scripted::new("codex");
    provider.script.lock().unwrap().history = vec![
        AgentDomainEvent::UserPromptSubmitted {
            text: "an old question".to_string(),
        },
        AgentDomainEvent::TurnStarted {
            turn_id: "old-1".to_string(),
        },
        AgentDomainEvent::AssistantMessageBoundary {
            turn_id: "old-1".to_string(),
        },
        AgentDomainEvent::ContentDelta {
            turn_id: "old-1".to_string(),
            kind: agent::ContentKind::Text,
            text: "an old answer".to_string(),
        },
        AgentDomainEvent::TurnCompleted {
            turn_id: "old-1".to_string(),
            outcome: agent::TurnOutcome::Completed,
            result_text: "an old answer".to_string(),
            stop_reason: None,
            usage: None,
            detail: Default::default(),
        },
    ];
    let lease = SessionLease::try_acquire("codex", &canonical_str, "thread-old").unwrap();
    let mut conversation = AgentConversation::resume_external(
        provider.clone(),
        start_for(&canonical, Some("thread-old")),
        &canonical,
        lease,
    )
    .expect("the resume is attached");
    {
        let projection = conversation.projection();
        assert!(
            projection.transcript.iter().any(|m| m.text.contains("an old answer")),
            "the history is in the projection: {:?}",
            projection.transcript
        );
        assert!(projection.active_turn_id.is_none(), "no past turn is running");
    }
    let mut delivered = Vec::new();
    for _ in 0..20 {
        if let agent::UiDelivery::Events(events) = conversation.take_ui_delivery() {
            delivered.extend(events);
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(
        !delivered.iter().any(|e| matches!(
            e,
            AgentDomainEvent::TurnStarted { .. }
                | AgentDomainEvent::TurnCompleted { .. }
                | AgentDomainEvent::UserPromptSubmitted { .. }
                | AgentDomainEvent::ContentDelta { .. }
        )),
        "a past turn was delivered as a live one: {delivered:#?}"
    );
    assert!(
        provider.script.lock().unwrap().history.is_empty(),
        "the history was taken once"
    );
    conversation.shutdown();
}

/// A past turn's ending is history: the band does not say it, and a live report of the account's limits
/// never writes today's reset time into it.
#[test]
fn a_resumed_ending_is_not_filled_by_a_live_limits_report() {
    let cwd = workspace("ext-resume-ending");
    let canonical = cwd.canonicalize().unwrap();
    let canonical_str = canonical.to_string_lossy().to_string();
    let provider = Scripted::new("codex");
    {
        let mut script = provider.script.lock().unwrap();
        script.history = vec![
            AgentDomainEvent::TurnStarted {
                turn_id: "old-1".to_string(),
            },
            AgentDomainEvent::TurnCompleted {
                turn_id: "old-1".to_string(),
                outcome: agent::TurnOutcome::LimitReached,
                result_text: "You've hit your usage limit.".to_string(),
                stop_reason: None,
                usage: None,
                detail: agent::TurnEndDetail {
                    origin_kind: None,
                    num_turns: None,
                    reason: Some("usage_limit".to_string()),
                    api_error_status: None,
                    message: None,
                },
            },
        ];
        script.events.push_back(AgentDomainEvent::SessionOpened {
            session_id: "ext-session".to_string(),
            provider_session_id: "thread-old".to_string(),
            model: "m".to_string(),
            cwd: canonical_str.clone(),
        });
        script.events.push_back(AgentDomainEvent::AccountLimits {
            used_percent: 3.0,
            window_minutes: Some(300),
            resets_at: Some(1_900_000_000),
            plan: None,
        });
    }
    let lease = SessionLease::try_acquire("codex", &canonical_str, "thread-old").unwrap();
    let mut conversation = AgentConversation::resume_external(
        provider.clone(),
        start_for(&canonical, Some("thread-old")),
        &canonical,
        lease,
    )
    .expect("the resume is attached");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while conversation.projection().account_limits.is_none() && std::time::Instant::now() < deadline {
        let _ = conversation.take_ui_delivery();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    {
        let projection = conversation.projection();
        assert!(projection.account_limits.is_some(), "the live report was folded");
        assert_eq!(projection.turn_endings.len(), 1, "the past ending is still a row");
        assert_eq!(
            projection.turn_endings[0].resets_at, None,
            "a live report filled a past turn's ending"
        );
        assert_eq!(projection.last_turn_ending, None, "the band says a past turn's ending");
    }
    conversation.shutdown();
}

#[test]
fn resume_external_refuses_a_lease_for_another_session() {
    let cwd = workspace("ext-resume-lease");
    let canonical = cwd.canonicalize().unwrap();
    let canonical_str = canonical.to_string_lossy().to_string();
    let provider = Scripted::new("codex");
    let lease = SessionLease::try_acquire("codex", &canonical_str, "some-other-thread").unwrap();
    let refused = AgentConversation::resume_external(
        provider.clone(),
        start_for(&canonical, Some("thread-2")),
        &canonical,
        lease,
    );
    assert!(matches!(refused, Err(ConversationError::Lease(_))));
    assert!(
        provider.script.lock().unwrap().started.is_empty(),
        "nothing was started"
    );
}

#[test]
fn resume_external_needs_a_session_to_resume() {
    let cwd = workspace("ext-resume-none");
    let canonical = cwd.canonicalize().unwrap();
    let provider = Scripted::new("codex");
    let lease = SessionLease::try_acquire("codex", &canonical.to_string_lossy(), "x").unwrap();
    let refused = AgentConversation::resume_external(provider, start_for(&canonical, None), &canonical, lease);
    assert!(matches!(refused, Err(ConversationError::Provider(_))));
}

/// The Claude door's source, kept in `agent`: the Claude sidecar provider must never name the
/// external extension, so that nothing about a Claude session can route through it.
#[test]
fn claude_sidecar_does_not_name_agent_external() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/providers/claude_sidecar");
    let mut files = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("the sidecar provider directory exists") {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "rs") {
            files.push(path);
        }
    }
    assert!(!files.is_empty(), "no source files under {dir:?}");
    let mut anchored = false;
    for path in files {
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {path:?}: {e}"));
        anchored |= text.contains("fn connect(");
        // Comment text may mention it; code may not.
        for (n, line) in text.lines().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            assert!(
                !code.contains("external::")
                    && !code.contains("ExternalProvider")
                    && !code.contains("ExternalStart")
                    && !code.contains("TurnPolicy"),
                "{}:{} names the external extension: {line}",
                path.display(),
                n + 1
            );
        }
    }
    assert!(
        anchored,
        "the scan did not reach `fn connect(`: the code it covers moved"
    );
}
