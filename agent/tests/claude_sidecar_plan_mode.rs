//! Plan mode through the sidecar provider and a conversation, against a stand-in sidecar
//! (`tests/support/sidecar_standin.rs --plan-mode`). No real `claude` runs here.
//!
//! What is checked: what a plan session states on the wire when it is created and resumed; that a plan
//! decision carries the mode and the digest, or the feedback, and nothing else; that a move into or out of
//! plan mode goes through `SetPlanning`, is confirmed by the CLI's report, and is undone when the sidecar
//! refuses it; and that a chat moving to another window carries its waiting plan and its plan-mode offer,
//! and does not move while a mode change is under way.
#![cfg(target_os = "linux")]

use agent::chat_mover::{self, MovedKind};
use agent::lease::SessionLease;
use agent::setting_sources::ProjectTrust;
use agent::{
    AgentConversation, AgentDomainEvent, AgentProvider, AgentSessionProjection, ClaudeSidecarProvider,
    CreateSessionRequest, InteractionResolution, MovedProvider, PlanExecutionMode, PlanOutcome, PlanPhase, PlanState,
    ProviderError, ProviderErrorCode, ResolveInteractionRequest, ResumeSessionRequest, SendTurnRequest,
    SetPlanningRequest, StreamingPreference,
};
use claude_runtime_protocol::v1::{CliPermissionMode, CreateSessionRequest as ProtoRequest};
use prost::Message as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long any one wait here may take before the test is called hung. Not a speed bound: every wait
/// ends as soon as what it waits for has happened.
const HANG: Duration = Duration::from_secs(30);

/// A private directory under the test target, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("plan-mode-{}", uuid::Uuid::new_v4()));
        agent::private_fs::create_private_dir_all(&dir, &dir).unwrap();
        Self(dir)
    }

    fn creates(&self) -> PathBuf {
        self.0.join("creates.bin")
    }

    fn answers(&self) -> PathBuf {
        self.0.join("answers.txt")
    }

    /// Every `CreateSession` the stand-in received, decoded.
    fn create_requests(&self) -> Vec<ProtoRequest> {
        let bytes = std::fs::read(self.creates()).unwrap_or_default();
        let mut requests = Vec::new();
        let mut at = 0;
        while at + 4 <= bytes.len() {
            let len = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
            requests.push(ProtoRequest::decode(&bytes[at + 4..at + 4 + len]).unwrap());
            at += 4 + len;
        }
        requests
    }

    /// The stand-in's record of plan decisions and `SetPlanning` calls, line by line.
    fn answer_lines(&self) -> Vec<String> {
        std::fs::read_to_string(self.answers())
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn answers_until(&self, done: impl Fn(&[String]) -> bool) -> Vec<String> {
        let started = Instant::now();
        loop {
            let lines = self.answer_lines();
            if done(&lines) {
                return lines;
            }
            assert!(
                started.elapsed() < HANG,
                "the stand-in never recorded it; it has {lines:?}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write_script(path: &Path, body: &str) {
    // Through a child process, so no write descriptor of this process is open for a forking test on
    // another thread to inherit: a script still open for writing fails to run with `ETXTBSY`.
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(r#"umask 077 && printf '%s' "$1" > "$2" && chmod 700 "$2""#)
        .arg("sh")
        .arg(body)
        .arg(path)
        .status()
        .expect("sh runs");
    assert!(status.success(), "writing the script failed: {status}");
}

/// A provider on the stand-in, recording into `scratch`, with `flags` on the stand-in's command line.
fn stand_in(scratch: &Scratch, flags: &str) -> ClaudeSidecarProvider {
    let quote = |path: &Path| format!("'{}'", path.display().to_string().replace('\'', "'\\''"));
    let launcher = scratch.0.join(format!("sidecar-{}", uuid::Uuid::new_v4()));
    let script = format!(
        "#!/bin/sh\nexec {} --record-create {} --record-answers {} {flags}\n",
        quote(Path::new(env!("CARGO_BIN_EXE_eitri-sidecar-standin"))),
        quote(&scratch.creates()),
        quote(&scratch.answers()),
    );
    write_script(&launcher, &script);
    ClaudeSidecarProvider::connect_to_program(&uuid::Uuid::new_v4().to_string(), &launcher)
        .expect("the stand-in sidecar starts and answers the handshake")
}

fn create(provider: &ClaudeSidecarProvider) -> String {
    provider
        .create_session(CreateSessionRequest {
            cwd: "/".into(),
            streaming: StreamingPreference::Partial,
            project: ProjectTrust::Untrusted,
            client_tools: None,
        })
        .expect("the stand-in opens a session")
}

/// Pumps `provider` into `seen` until `done` holds for what was seen.
fn pump_until(
    provider: &ClaudeSidecarProvider,
    seen: &mut Vec<AgentDomainEvent>,
    what: &str,
    done: impl Fn(&[AgentDomainEvent]) -> bool,
) {
    let started = Instant::now();
    loop {
        seen.extend(provider.pump());
        if done(seen) {
            return;
        }
        assert!(started.elapsed() < HANG, "never saw {what}; saw {seen:#?}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// A conversation on a plan-mode stand-in.
struct Chat {
    conversation: AgentConversation,
    provider: Arc<ClaudeSidecarProvider>,
    scratch: Scratch,
}

impl Chat {
    fn new(name: &str) -> Self {
        let scratch = Scratch::new();
        let provider = Arc::new(stand_in(&scratch, "--plan-mode"));
        let cwd = agent::state_dirs::test_workspace_dir(name);
        let conversation =
            AgentConversation::create(provider.clone(), &cwd, ProjectTrust::Untrusted).expect("a conversation opens");
        let chat = Self {
            conversation,
            provider,
            scratch,
        };
        chat.wait("the session opening", |p| p.session_id.is_some());
        chat
    }

    fn wait(&self, what: &str, done: impl Fn(&AgentSessionProjection) -> bool) {
        wait_on(&self.conversation, what, done);
    }

    /// Sends `script` as a turn and waits for its end.
    fn run(&mut self, script: &str) {
        let before = self.conversation.projection().last_turn_end_seq;
        self.conversation
            .send_turn(script)
            .expect("the stand-in takes the turn");
        self.wait("the turn's end", |p| {
            p.active_turn_id.is_none() && p.last_turn_end_seq.is_some() && p.last_turn_end_seq != before
        });
    }

    /// Enters plan mode through `SetPlanning` and waits for the CLI's report of it.
    fn enter(&mut self) {
        let send = self
            .conversation
            .set_planning(true, now_ms())
            .expect("the entry goes out");
        assert_eq!(send.wait(HANG).expect("the sidecar acknowledges it"), "plan");
        self.wait("the CLI to report plan", |p| p.planning.phase == PlanPhase::Planning);
    }

    /// Has the model propose `text` as its plan, in a turn left open until the plan is settled.
    fn propose(&mut self, text: &str) -> (String, String) {
        self.conversation
            .send_turn(&format!("plan {text}; end"))
            .expect("the stand-in takes the turn");
        self.wait("the plan", |p| p.waiting_plan().is_some());
        let projection = self.conversation.projection();
        let plan = projection.waiting_plan().unwrap();
        assert_eq!(plan.plan, text);
        (plan.interaction_id.clone(), plan.plan_sha256.clone())
    }
}

fn wait_on(conversation: &AgentConversation, what: &str, done: impl Fn(&AgentSessionProjection) -> bool) {
    let started = Instant::now();
    loop {
        let _ = conversation.take_ui_delivery();
        if done(&conversation.projection()) {
            return;
        }
        assert!(
            started.elapsed() < HANG,
            "never saw {what}; projection {:#?}",
            *conversation.projection()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A session on a sidecar offering plan mode asks for it, and states the CLI's mode with it, on a fresh
/// session and on a resume alike; its reports are judged with the offer. Without the capability, or with
/// the host not ready, neither is sent and the request is the one sent before plan mode existed.
#[test]
fn a_plan_session_states_plan_mode_on_create_and_resume() {
    let scratch = Scratch::new();
    let provider = stand_in(&scratch, "--plan-mode");
    create(&provider);
    assert!(provider.cli_expectation().plan_offered);
    provider
        .resume_session(ResumeSessionRequest {
            provider_session_id: "claude-earlier".into(),
            cwd: "/".into(),
            streaming: StreamingPreference::Partial,
            project: ProjectTrust::Untrusted,
            client_tools: None,
        })
        .expect("the stand-in resumes");
    let requests = scratch.create_requests();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert!(request.client_interactions.as_ref().is_some_and(|c| c.plan_mode));
        let policy = request.policy.as_ref().unwrap();
        assert_eq!(policy.cli_permission_mode, CliPermissionMode::Default as i32);
        assert!(!policy.permission_mode_switchable);
    }
    assert_eq!(
        requests[1].resume_provider_session_id.as_deref(),
        Some("claude-earlier")
    );

    for (flags, ready) in [("", true), ("--plan-mode", false)] {
        let scratch = Scratch::new();
        let mut provider = stand_in(&scratch, flags);
        provider.set_plan_mode_override_for_tests(ready);
        create(&provider);
        assert!(!provider.cli_expectation().plan_offered, "{flags} {ready}");
        let request = &scratch.create_requests()[0];
        assert!(!request.client_interactions.as_ref().is_some_and(|c| c.plan_mode));
        assert_eq!(
            request.policy.as_ref().unwrap().cli_permission_mode,
            CliPermissionMode::Unspecified as i32
        );
    }
}

/// An approval sends the mode and the digest of the plan shown, nothing else; the plan settles from the
/// sidecar's event with the mode named, and the move out of plan mode, started before the send, ends with
/// the CLI's report of its creation mode. A digest that is not the plan's is refused before anything goes.
#[test]
fn an_approval_goes_as_mode_and_digest_only() {
    let mut chat = Chat::new("plan-approve");
    chat.enter();
    let (id, digest) = chat.propose("# Plan: edit the file");

    let wrong = chat.conversation.resolve_plan(
        &id,
        InteractionResolution::PlanApprove {
            mode: PlanExecutionMode::Auto,
            plan_sha256: "0".repeat(64),
        },
    );
    assert!(wrong.is_err(), "a digest that is not the plan's is refused here");
    assert!(chat
        .scratch
        .answer_lines()
        .iter()
        .all(|line| !line.starts_with("plan ")));
    assert_eq!(chat.conversation.projection().planning.phase, PlanPhase::Planning);

    let send = chat
        .conversation
        .resolve_plan(
            &id,
            InteractionResolution::PlanApprove {
                mode: PlanExecutionMode::Auto,
                plan_sha256: digest.clone(),
            },
        )
        .expect("the approval goes out");
    assert!(
        matches!(
            chat.conversation.projection().planning.phase,
            PlanPhase::Executing | PlanPhase::Leaving { .. }
        ),
        "the move out started with the send"
    );
    assert_eq!(send.wait(HANG).expect("the sidecar takes it"), None);
    let lines = chat
        .scratch
        .answers_until(|lines| lines.iter().any(|l| l.starts_with("plan ")));
    assert!(lines.contains(&format!("plan {id} approve 2 {digest}")), "{lines:?}");
    chat.wait("the settlement and the CLI's report", |p| {
        p.planning.phase == PlanPhase::Executing
            && matches!(
                p.plan(&id).map(|plan| &plan.state),
                Some(PlanState::Settled {
                    outcome: PlanOutcome::Approved,
                    mode_after: Some(PlanExecutionMode::Auto),
                    ..
                })
            )
    });
    assert!(chat.conversation.projection().ungated_cli_mode.is_none());
    chat.conversation.shutdown();
}

/// "Keep planning" sends the feedback exactly as written; the plan settles with it and the session stays in
/// plan mode. Feedback over its bound is refused before anything goes.
#[test]
fn keep_planning_goes_with_its_feedback() {
    let mut chat = Chat::new("plan-keep");
    chat.enter();
    let (id, _) = chat.propose("# Plan");
    let too_long = chat.conversation.resolve_plan(
        &id,
        InteractionResolution::PlanKeep {
            feedback: "x".repeat(16 * 1024 + 1),
        },
    );
    assert!(too_long.is_err());
    let send = chat
        .conversation
        .resolve_plan(
            &id,
            InteractionResolution::PlanKeep {
                feedback: "shorter, please".into(),
            },
        )
        .expect("the decision goes out");
    assert_eq!(send.wait(HANG).expect("the sidecar takes it"), None);
    let lines = chat
        .scratch
        .answers_until(|lines| lines.iter().any(|l| l.starts_with("plan ")));
    assert_eq!(
        lines.iter().filter(|l| l.starts_with("plan ")).collect::<Vec<_>>(),
        [&format!("plan {id} keep shorter, please")]
    );
    chat.wait("the settlement", |p| {
        matches!(
            p.plan(&id).map(|plan| &plan.state),
            Some(PlanState::Settled {
                outcome: PlanOutcome::KeptPlanning,
                feedback: Some(feedback),
                ..
            }) if feedback == "shorter, please"
        )
    });
    assert_eq!(chat.conversation.projection().planning.phase, PlanPhase::Planning);
    chat.conversation.shutdown();
}

/// A sidecar without `plan_mode_v1`, or a session that did not offer plan mode, refuses `SetPlanning` and a
/// plan decision before anything is sent.
#[test]
fn set_planning_without_the_capability_is_refused_with_nothing_sent() {
    let scratch = Scratch::new();
    let provider = stand_in(&scratch, "");
    let session = create(&provider);
    let refused = provider
        .set_planning(SetPlanningRequest {
            session_id: session.clone(),
            enter: true,
        })
        .map(|_| ())
        .unwrap_err();
    assert!(matches!(refused, ProviderError::UnsupportedCapability("plan_mode_v1")));
    let refused = provider
        .resolve_interaction(ResolveInteractionRequest {
            session_id: session,
            interaction_id: "plan-1".into(),
            resolution: InteractionResolution::PlanKeep {
                feedback: String::new(),
            },
        })
        .map(|_| ())
        .unwrap_err();
    assert!(matches!(refused, ProviderError::UnsupportedCapability("plan_mode_v1")));

    let mut not_offered = stand_in(&scratch, "--plan-mode");
    not_offered.set_plan_mode_override_for_tests(false);
    let session = create(&not_offered);
    let refused = not_offered
        .set_planning(SetPlanningRequest {
            session_id: session,
            enter: true,
        })
        .map(|_| ())
        .unwrap_err();
    assert!(matches!(refused, ProviderError::UnsupportedCapability("plan_mode_v1")));
    assert!(scratch.answer_lines().is_empty(), "{:?}", scratch.answer_lines());

    let cwd = agent::state_dirs::test_workspace_dir("plan-unoffered");
    let mut conversation = AgentConversation::create(Arc::new(stand_in(&scratch, "")), &cwd, ProjectTrust::Untrusted)
        .expect("a conversation opens");
    let refused = conversation.set_planning(true, now_ms()).map(|_| ()).unwrap_err();
    assert!(refused.is_benign(), "{refused}");
    assert_eq!(conversation.projection().planning.phase, PlanPhase::Executing);
    conversation.shutdown();
}

/// A `SetPlanning` the sidecar refuses is benign, and the session goes back to where the move started: the
/// guard has nothing left to fail. The sidecar's own deadline is not a refusal: the move stays open, and the
/// CLI's report of it still confirms it.
#[test]
fn a_refused_set_planning_is_benign_and_undoes_the_move() {
    let mut chat = Chat::new("plan-refused");
    chat.run("refuse-planning planning_cli_refused; end");
    let send = chat
        .conversation
        .set_planning(true, now_ms())
        .expect("the entry goes out");
    assert!(matches!(
        chat.conversation.projection().planning.phase,
        PlanPhase::Entering { .. }
    ));
    let error = send.wait(HANG).unwrap_err();
    assert!(error.is_benign(), "{error}");
    assert!(matches!(
        &error,
        ProviderError::Provider {
            code: ProviderErrorCode::PlanningRefused,
            message,
        } if message.starts_with("Claude Code refused")
    ));
    chat.conversation.planning_send_failed(true, &error);
    let mut projection = chat.conversation.projection().clone();
    assert_eq!(projection.planning.phase, PlanPhase::Executing);
    assert_eq!(projection.plan_transition_overdue(u64::MAX), None);

    // A turn running is a refusal too.
    chat.run("refuse-planning planning_turn_running; end");
    let send = chat
        .conversation
        .set_planning(true, now_ms())
        .expect("the entry goes out");
    let error = send.wait(HANG).unwrap_err();
    assert!(matches!(
        error,
        ProviderError::Provider {
            code: ProviderErrorCode::PlanningRefused,
            ..
        }
    ));
    chat.conversation.planning_send_failed(true, &error);
    assert_eq!(chat.conversation.projection().planning.phase, PlanPhase::Executing);

    // The sidecar's deadline: the move stays open, and the CLI's report confirms it.
    chat.run("slow-planning; end");
    let send = chat
        .conversation
        .set_planning(true, now_ms())
        .expect("the entry goes out");
    let error = send.wait(HANG).unwrap_err();
    assert!(matches!(
        error,
        ProviderError::Provider {
            code: ProviderErrorCode::DeadlineExceeded,
            ..
        }
    ));
    chat.conversation.planning_send_failed(true, &error);
    chat.wait("the CLI's report of plan", |p| p.planning.phase == PlanPhase::Planning);

    // And out again: the creation mode reported ends the move.
    let send = chat
        .conversation
        .set_planning(false, now_ms())
        .expect("the leave goes out");
    assert_eq!(send.wait(HANG).unwrap(), "default");
    chat.wait("the CLI's report of default", |p| {
        p.planning.phase == PlanPhase::Executing
    });
    assert!(chat.conversation.projection().ungated_cli_mode.is_none());
    let lines = chat.scratch.answer_lines();
    assert_eq!(
        lines,
        [
            "set-planning true refused planning_cli_refused",
            "set-planning true refused planning_turn_running",
            "set-planning true plan",
            "set-planning false default",
        ]
    );
    chat.conversation.shutdown();
}

/// A `SetPlanning` the sidecar answers late is not overdue while its reply is unread, however long that
/// takes on the host's clock: the guard counts from the reply, and the CLI's report confirms the move.
#[test]
fn a_late_set_planning_reply_holds_the_guard_until_it_is_read() {
    let mut chat = Chat::new("plan-late");
    chat.run("late-planning 400; end");
    let started = now_ms();
    let send = chat
        .conversation
        .set_planning(true, started)
        .expect("the entry goes out");
    assert!(matches!(
        chat.conversation.projection().planning.phase,
        PlanPhase::Entering { .. }
    ));
    assert_eq!(
        chat.conversation.plan_transition_overdue(started + 60_000),
        None,
        "the reply is not in yet"
    );
    assert_eq!(send.wait(HANG).expect("the sidecar acknowledges it"), "plan");
    chat.conversation.planning_send_taken(true);
    chat.wait("the CLI to report plan", |p| p.planning.phase == PlanPhase::Planning);
    assert_eq!(chat.conversation.plan_transition_overdue(u64::MAX), None);
    assert!(chat.conversation.projection().ungated_cli_mode.is_none());
    chat.conversation.shutdown();
}

/// A plan decision whose reply has not been read holds the sidecar in this window: a refused decision has
/// to put the plan back where it was asked. So does a `SetPlanning` whose reply is unread.
#[test]
fn an_approval_whose_reply_is_unread_refuses_a_freeze() {
    let scratch = Scratch::new();
    let provider = stand_in(&scratch, "--plan-mode");
    let session = create(&provider);
    let mut seen = Vec::new();
    pump_until(&provider, &mut seen, "the session opening", |seen| {
        seen.iter().any(|e| matches!(e, AgentDomainEvent::SessionOpened { .. }))
    });
    let entering = provider
        .set_planning(SetPlanningRequest {
            session_id: session.clone(),
            enter: true,
        })
        .unwrap();
    assert!(matches!(
        provider.freeze_for_move(),
        Err(agent::providers::claude_sidecar::MoveRefusedWhy::AnswerInFlight)
    ));
    assert_eq!(entering.wait(HANG).unwrap(), "plan");
    drop(entering);
    provider
        .send_turn(SendTurnRequest::text(session.clone(), "plan # Plan; end".to_string()))
        .unwrap();
    pump_until(&provider, &mut seen, "the plan", |seen| {
        seen.iter().any(|e| matches!(e, AgentDomainEvent::PlanProposed { .. }))
    });
    let (id, digest) = seen
        .iter()
        .find_map(|e| match e {
            AgentDomainEvent::PlanProposed {
                interaction_id,
                plan_sha256,
                ..
            } => Some((interaction_id.clone(), plan_sha256.clone())),
            _ => None,
        })
        .unwrap();
    let approving = provider
        .resolve_interaction(ResolveInteractionRequest {
            session_id: session,
            interaction_id: id,
            resolution: InteractionResolution::PlanApprove {
                mode: PlanExecutionMode::Manual,
                plan_sha256: digest,
            },
        })
        .unwrap();
    assert!(matches!(
        provider.freeze_for_move(),
        Err(agent::providers::claude_sidecar::MoveRefusedWhy::AnswerInFlight)
    ));
    assert_eq!(approving.wait(HANG).unwrap(), None);
    drop(approving);
    let (handoff, _fds) = provider.freeze_for_move().expect("nothing is in flight any more");
    assert!(handoff.plan_offered);
    provider.unfreeze().unwrap();
}

/// Freezes `conversation` for a move, asking again while its ingest folds.
fn freeze(
    conversation: &mut AgentConversation,
) -> Result<(agent::ChatMoveCore, agent::ConversationHandoff), agent::HandoffRefused> {
    let deadline = Instant::now() + HANG;
    loop {
        match conversation.freeze_for_move(|c| {
            let _ = c.take_ui_delivery();
            Ok(())
        }) {
            Err(agent::HandoffRefused::Folding) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(2))
            }
            other => return other,
        }
    }
}

/// Moves `chat`'s conversation to a new one in this process, the way another window takes it up.
fn move_chat(conversation: &mut AgentConversation) -> AgentConversation {
    let (core, handoff) = freeze(conversation).expect("the conversation freezes");
    let mover = conversation.mover().expect("a sidecar chat moves");
    let frozen = mover.freeze_for_chat_move().expect("the sidecar is handed over");
    let lease = conversation.lease_descriptor().and_then(Result::ok);
    let session = handoff.provider_session_id.clone().unwrap_or_default();
    let cwd = handoff.canonical_cwd.to_string_lossy().into_owned();
    let record: serde_json::Value = serde_json::from_str(&frozen.record.to_string()).unwrap();
    assert_eq!(
        record["plan_offered"],
        serde_json::json!(true),
        "the handoff carries the offer"
    );
    let adopted = chat_mover::adopt(MovedKind::Claude, &record, frozen.copies().unwrap()).expect("taken up");
    let lease = lease.map(|fd| SessionLease::from_moved(fd, &handoff.provider_name, &cwd, &session).unwrap());
    let mut taken = AgentConversation::adopt_moved(
        core,
        handoff,
        MovedProvider {
            agent: adopted.agent,
            external: None,
            lease,
        },
    )
    .expect("the conversation is rebuilt");
    taken.mover().unwrap().commit_chat_move().expect("the move commits");
    let _ = taken.commit_moved();
    mover.forget_chat_move();
    drop(frozen);
    taken
}

/// A plan waiting for a decision crosses a move with its session, and the window that took it up decides it.
#[test]
fn a_move_carries_the_waiting_plan() {
    let mut chat = Chat::new("plan-move");
    chat.enter();
    let (id, digest) = chat.propose("# Plan to move");
    let mut taken = move_chat(&mut chat.conversation);
    {
        let projection = taken.projection();
        let plan = projection.waiting_plan().expect("the plan crossed");
        assert_eq!(
            (plan.interaction_id.as_str(), plan.plan_sha256.as_str()),
            (id.as_str(), digest.as_str())
        );
        assert_eq!(projection.planning.phase, PlanPhase::Planning);
    }
    let send = taken
        .resolve_plan(
            &id,
            InteractionResolution::PlanApprove {
                mode: PlanExecutionMode::Manual,
                plan_sha256: digest,
            },
        )
        .expect("the adopting window decides it");
    assert_eq!(send.wait(HANG).unwrap(), None);
    wait_on(&taken, "the settlement and the move out", |p| {
        p.planning.phase == PlanPhase::Executing && p.waiting_plan().is_none()
    });
    assert!(taken.projection().ungated_cli_mode.is_none());
    taken.shutdown();
    drop(chat.provider);
}

/// The adopting window judges the CLI's `plan` reports as the giving window did: a move into plan mode made
/// there is accepted, not taken for an unexpected mode.
#[test]
fn a_move_carries_plan_offered_and_the_adopter_accepts_plan_reports() {
    let mut chat = Chat::new("plan-move-offer");
    let mut taken = move_chat(&mut chat.conversation);
    assert!(taken.projection().planning.offered);
    let send = taken
        .set_planning(true, now_ms())
        .expect("the adopting window enters plan mode");
    assert_eq!(send.wait(HANG).unwrap(), "plan");
    wait_on(&taken, "the CLI's report of plan", |p| {
        p.planning.phase == PlanPhase::Planning
    });
    assert!(taken.projection().ungated_cli_mode.is_none());
    taken.shutdown();
}

/// While a move into or out of plan mode waits for the CLI's report the chat does not move: the report, a
/// refusal or the guard settles it in the window that started it.
#[test]
fn a_move_during_a_transition_is_refused() {
    let mut chat = Chat::new("plan-move-transition");
    // The stand-in reports a move at once, so the move this window started is folded without sending it:
    // the window waits for a report that will not come.
    chat.conversation.fold_locally(AgentDomainEvent::PlanTransitionStarted {
        enter: true,
        cause: agent::PlanCause::Host,
        at_ms: now_ms(),
    });
    assert!(matches!(
        chat.conversation.projection().planning.phase,
        PlanPhase::Entering { .. }
    ));
    let refused = freeze(&mut chat.conversation).map(|_| ()).unwrap_err();
    assert!(matches!(refused, agent::HandoffRefused::ModeChanging), "{refused}");
    assert_eq!(
        refused.to_string(),
        "the mode is changing; move the tab when it settles"
    );
    // Once it settles the chat moves.
    chat.conversation.planning_send_failed(
        true,
        &ProviderError::Provider {
            code: ProviderErrorCode::PlanningRefused,
            message: "no".into(),
        },
    );
    assert_eq!(chat.conversation.projection().planning.phase, PlanPhase::Executing);
    let taken = move_chat(&mut chat.conversation);
    drop(taken);
}
