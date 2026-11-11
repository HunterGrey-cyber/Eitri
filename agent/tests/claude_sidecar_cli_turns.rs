//! Turns Claude Code starts by itself, and a prompt it reads only after work of its own, through the
//! sidecar provider and a conversation, against a stand-in sidecar
//! (`tests/support/sidecar_standin.rs --cli-turns`) that plays each shape. No real `claude` runs here.
//!
//! What is checked: the events the provider hands over, the conversation's projection after folding
//! them, and what the provider puts on the wire for a sent or steered message (its client id, and the
//! mark of a message the person typed, sent only to a sidecar that accepts it).
#![cfg(target_os = "linux")]

use agent::setting_sources::ProjectTrust;
use agent::{
    AgentConversation, AgentDomainEvent, AgentProvider, AgentSessionProjection, ClaudeSidecarProvider,
    CreateSessionRequest, PromptOrigin, SendTurnRequest, SteerTurnRequest, StreamingPreference, TurnCauseKind,
    TurnOrigin, TurnStamp,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long any one wait here may take before the test is called hung. Not a speed bound: every wait
/// ends as soon as what it waits for has happened.
const HANG: Duration = Duration::from_secs(30);

/// A private directory under the test target, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("cli-turns-{name}-{}", uuid::Uuid::new_v4()));
        agent::private_fs::create_private_dir_all(&dir, &dir).unwrap();
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Writes an executable script through a child process. Written from this process, the file's write
/// descriptor would be open here while a test on another thread forks; the forked child holds it until
/// it execs, and running the script meanwhile fails with `ETXTBSY` ("Text file busy").
fn write_script(path: &Path, body: &str) {
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

/// A provider on the stand-in, which records every `SendTurn` and `SteerTurn` into `scratch`'s
/// `sends.txt`. `cli_turns`: whether it offers protocol 3.22's `cli_turns` and `human_origin`.
fn stand_in(scratch: &Scratch, cli_turns: bool) -> ClaudeSidecarProvider {
    let quote = |path: &Path| format!("'{}'", path.display().to_string().replace('\'', "'\\''"));
    let launcher = scratch.0.join("sidecar");
    let script = format!(
        "#!/bin/sh\nexec {} --record-sends {} {}\n",
        quote(Path::new(env!("CARGO_BIN_EXE_eitri-sidecar-standin"))),
        quote(&scratch.0.join("sends.txt")),
        if cli_turns { "--cli-turns" } else { "" }
    );
    write_script(&launcher, &script);
    let provider = ClaudeSidecarProvider::connect_to_program(&uuid::Uuid::new_v4().to_string(), &launcher)
        .expect("the stand-in sidecar starts and answers the handshake");
    assert_eq!(provider.capabilities().cli_turns, cli_turns);
    assert_eq!(provider.capabilities().human_origin, cli_turns);
    provider
}

/// The recorded `<rpc> <client id or -> <typed>` lines, once there are `count` of them.
fn sends(scratch: &Scratch, count: usize) -> Vec<String> {
    let started = Instant::now();
    loop {
        let lines: Vec<String> = std::fs::read_to_string(scratch.0.join("sends.txt"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect();
        if lines.len() >= count {
            return lines;
        }
        assert!(started.elapsed() < HANG, "only {lines:?} recorded");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A conversation on the stand-in, with what its ingestion hands the UI collected as it goes.
struct Chat {
    conversation: AgentConversation,
    delivered: Vec<AgentDomainEvent>,
    _scratch: Scratch,
}

impl Chat {
    fn new(name: &str) -> Self {
        let scratch = Scratch::new(name);
        let provider = Arc::new(stand_in(&scratch, true));
        let cwd = agent::state_dirs::test_workspace_dir(name);
        let conversation =
            AgentConversation::create(provider, &cwd, ProjectTrust::Untrusted).expect("a conversation opens");
        let mut chat = Self {
            conversation,
            delivered: Vec::new(),
            _scratch: scratch,
        };
        chat.wait("the session opening", |p| p.session_id.is_some());
        chat
    }

    /// Sends `script` as the host sends a prompt the person typed as `as_typed`: the prompt folded first,
    /// then the send under its client id.
    fn send(&mut self, script: &str, as_typed: &str, client_id: &str) {
        self.conversation.fold_locally(AgentDomainEvent::UserPromptSubmitted {
            text: as_typed.to_string(),
        });
        self.conversation
            .send_turn_as(script, as_typed, PromptOrigin::Typed, client_id)
            .expect("the stand-in takes the turn");
    }

    /// Waits until the projection satisfies `done` and the UI has every event folded so far.
    fn wait(&mut self, what: &str, done: impl Fn(&AgentSessionProjection) -> bool) {
        let started = Instant::now();
        loop {
            match self.conversation.take_ui_delivery() {
                agent::UiDelivery::Events(events) => self.delivered.extend(events),
                agent::UiDelivery::Resync => panic!("a resync in a test this small"),
                agent::UiDelivery::Nothing => {}
            }
            if done(&self.conversation.projection()) {
                return;
            }
            assert!(
                started.elapsed() < HANG,
                "never saw {what}; projection {:#?}",
                *self.conversation.projection()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Waits until `turns` turns have completed and none runs.
    fn wait_turns(&mut self, turns: usize) {
        let started = Instant::now();
        loop {
            self.wait("the next delivery", |_| true);
            if self.completed() >= turns && self.conversation.projection().active_turn_id.is_none() {
                return;
            }
            assert!(
                started.elapsed() < HANG,
                "never saw {turns} turns end: {:#?}",
                self.delivered
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn completed(&self) -> usize {
        self.delivered
            .iter()
            .filter(|event| matches!(event, AgentDomainEvent::TurnCompleted { .. }))
            .count()
    }

    fn texts(&self) -> Vec<(String, bool)> {
        self.conversation
            .projection()
            .user_prompts
            .iter()
            .map(|p| (p.text.clone(), p.delivered))
            .collect()
    }

    fn starts(&self) -> Vec<(TurnOrigin, Option<TurnCauseKind>)> {
        self.conversation
            .projection()
            .turn_starts
            .iter()
            .map(|s| (s.origin, s.cause.as_ref().map(|c| c.kind)))
            .collect()
    }

    /// Every turn started at most once, as the panel must fold it.
    fn each_turn_started_once(&self) {
        let mut seen = std::collections::BTreeSet::new();
        for event in &self.delivered {
            if let AgentDomainEvent::TurnStarted { turn_id } = event {
                assert!(
                    seen.insert(turn_id.clone()),
                    "{turn_id} started twice: {:#?}",
                    self.delivered
                );
            }
        }
    }
}

#[test]
fn a_turn_claude_code_starts_is_a_turn_of_its_own_with_its_cause() {
    let mut chat = Chat::new("cli-turn");
    chat.send(
        "text m1 Started.; end; cli-start task; text m2 The tests passed.; end",
        "run the tests in the background",
        "eitri-1",
    );
    chat.wait_turns(2);
    chat.each_turn_started_once();
    let starts: Vec<_> = chat
        .delivered
        .iter()
        .filter_map(|event| match event {
            AgentDomainEvent::TurnAttributed { turn_id, origin, .. } => Some((turn_id.clone(), *origin)),
            _ => None,
        })
        .collect();
    assert_eq!(starts.len(), 1, "{:#?}", chat.delivered);
    assert_eq!(starts[0].1, TurnOrigin::Cli);
    assert_eq!(chat.starts(), [(TurnOrigin::Cli, Some(TurnCauseKind::Task))]);
    let projection = chat.conversation.projection();
    let start = &projection.turn_starts[0];
    assert_eq!(start.turn_id, starts[0].0);
    assert_eq!(start.cause.as_ref().unwrap().description.as_deref(), Some("cargo test"));
    let replies: Vec<_> = projection.transcript.iter().map(|m| m.text.as_str()).collect();
    assert_eq!(replies, ["Started.", "The tests passed."], "two turns, two replies");
    assert!(start.seq > projection.transcript[0].seq && start.seq < projection.transcript[1].seq);
}

#[test]
fn a_cause_named_at_the_close_replaces_the_one_the_start_gave() {
    let mut chat = Chat::new("cause-update");
    chat.send(
        "end; cli-start unknown; text m1 Looked.; cause peer; end",
        "wait for the reviewer",
        "eitri-1",
    );
    chat.wait_turns(2);
    assert_eq!(chat.starts(), [(TurnOrigin::Cli, Some(TurnCauseKind::Peer))]);
    let projection = chat.conversation.projection();
    let cause = projection.turn_starts[0].cause.as_ref().unwrap();
    assert_eq!(cause.peer_name.as_deref(), Some("reviewer"));
    assert_eq!(cause.text.as_deref(), Some("look at line 4"), "the peer's body");
}

#[test]
fn a_prompt_claude_code_reads_after_work_of_its_own_moves_where_it_was_read() {
    let mut chat = Chat::new("case-two-read");
    chat.send(
        "cycle task; text m1 The build finished.; sleep 200; deliver; text m2 Docs updated.; end",
        "and the docs",
        "eitri-7",
    );
    chat.wait("the cycle", |p| !p.turn_starts.is_empty());
    let delivered_yet = chat
        .delivered
        .iter()
        .any(|event| matches!(event, AgentDomainEvent::UserMessageDelivered { .. }));
    if !delivered_yet {
        assert!(
            chat.texts().is_empty(),
            "the prompt is out of the conversation while Claude Code works ahead of it: {:?}",
            chat.texts()
        );
    }
    chat.wait_turns(1);
    chat.each_turn_started_once();
    let delivery = chat
        .delivered
        .iter()
        .find_map(|event| match event {
            AgentDomainEvent::UserMessageDelivered {
                client_message_id,
                text,
                ..
            } => Some((client_message_id.clone(), text.clone())),
            _ => None,
        })
        .expect("the delivery is handed over");
    assert_eq!(
        delivery,
        ("eitri-7".to_string(), "and the docs".to_string()),
        "named by its id, shown as typed"
    );
    assert_eq!(chat.texts(), [("and the docs".to_string(), true)]);
    assert_eq!(chat.starts(), [(TurnOrigin::Cli, Some(TurnCauseKind::Task))]);
    let projection = chat.conversation.projection();
    let woken = projection
        .transcript
        .iter()
        .find(|m| m.text == "The build finished.")
        .unwrap();
    assert!(projection.user_prompts[0].seq > woken.seq, "read after the woken work");
    assert_eq!(
        projection.steer_outcome("eitri-7"),
        Some(&agent::SteerOutcome::Delivered {
            seq: projection.user_prompts[0].seq
        })
    );
}

#[test]
fn a_prompt_claude_code_drops_after_work_of_its_own_is_handed_back_to_the_host() {
    let mut chat = Chat::new("case-two-drop");
    chat.send(
        "cycle scheduled; text m1 Checked.; drop; end",
        "and the docs",
        "eitri-8",
    );
    chat.wait_turns(1);
    assert!(chat.delivered.iter().any(|event| matches!(
        event,
        AgentDomainEvent::UserMessageDropped { client_message_id, .. } if client_message_id == "eitri-8"
    )));
    assert!(chat.texts().is_empty(), "the tab holds it now, not the conversation");
    assert!(matches!(
        chat.conversation.projection().steer_outcome("eitri-8"),
        Some(agent::SteerOutcome::Dropped { .. })
    ));
}

#[test]
fn an_ending_without_a_result_keeps_its_row_unless_the_turn_drew_nothing() {
    let mut chat = Chat::new("no-result");
    chat.send(
        "end; cli-start task; end-no-result; cli-start task; text m1 Looking; end-no-result",
        "go",
        "eitri-1",
    );
    chat.wait_turns(3);
    let projection = chat.conversation.projection();
    assert_eq!(
        projection.turn_starts.len(),
        1,
        "the empty one left no row: {:#?}",
        projection.turn_starts
    );
    assert_eq!(projection.turn_endings.len(), 1);
    assert_eq!(
        projection.turn_endings[0].detail.reason.as_deref(),
        Some(agent::NO_RESULT_REASON)
    );
    assert_eq!(projection.turn_endings[0].turn_id, projection.turn_starts[0].turn_id);
}

fn open(provider: &ClaudeSidecarProvider) -> String {
    provider
        .create_session(CreateSessionRequest {
            cwd: "/".to_string(),
            streaming: StreamingPreference::Partial,
            project: ProjectTrust::Untrusted,
            client_tools: None,
        })
        .expect("the stand-in opens a session")
}

fn stamp(id: Option<&str>, typed: bool) -> TurnStamp {
    TurnStamp {
        client_message_id: id.map(str::to_string),
        typed_by_person: typed,
        as_typed: None,
    }
}

/// Waits until a turn has started, and returns its id.
fn started(provider: &ClaudeSidecarProvider) -> String {
    let begun = Instant::now();
    loop {
        for event in provider.pump() {
            if let AgentDomainEvent::TurnStarted { turn_id } = event {
                return turn_id;
            }
        }
        assert!(begun.elapsed() < HANG, "no turn started");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_send_carries_the_prompts_client_id_and_the_typed_mark_only_where_the_sidecar_accepts_them() {
    for accepts in [true, false] {
        let scratch = Scratch::new("wire");
        let provider = stand_in(&scratch, accepts);
        let session = open(&provider);
        let send = |script: &str, stamp: TurnStamp| {
            provider
                .send_turn_stamped(
                    SendTurnRequest {
                        session_id: session.clone(),
                        text: script.to_string(),
                    },
                    stamp,
                )
                .expect("the turn is taken")
        };
        send("end", stamp(Some("eitri-1"), true));
        let _ = started(&provider);
        send("end", stamp(Some("eitri-2"), false));
        let _ = started(&provider);
        provider
            .send_turn(SendTurnRequest {
                session_id: session.clone(),
                text: "ask hold; end".to_string(),
            })
            .expect("the turn is taken");
        let turn = started(&provider);
        let steer = |id: &str, typed: bool| {
            provider
                .steer_turn_stamped(
                    SteerTurnRequest {
                        session_id: session.clone(),
                        expected_turn_id: turn.clone(),
                        client_message_id: id.to_string(),
                        text: "10".to_string(),
                        as_typed: "and this".to_string(),
                    },
                    stamp(None, typed),
                )
                .expect("the steer is taken")
        };
        steer("eitri-3", true);
        let _ = sends(&scratch, 4);
        steer("eitri-4", false);
        let lines = sends(&scratch, 5);
        let expected: Vec<&str> = if accepts {
            vec![
                "send eitri-1 true",
                "send eitri-2 false",
                "send - false",
                "steer eitri-3 true",
                "steer eitri-4 false",
            ]
        } else {
            vec![
                "send - false",
                "send - false",
                "send - false",
                "steer eitri-3 false",
                "steer eitri-4 false",
            ]
        };
        assert_eq!(lines, expected, "sidecar accepts the 3.22 fields: {accepts}");
    }
}

/// The CLI reads an `@path` in any text it gets: only a person's own line may do that.
#[test]
fn a_mention_reaches_the_cli_as_one_only_in_what_a_person_typed() {
    let scratch = Scratch::new("mentions");
    let provider = stand_in(&scratch, true);
    let session = open(&provider);
    let send = |typed: bool| {
        provider
            .send_turn_stamped(
                SendTurnRequest {
                    session_id: session.clone(),
                    text: "end; @/home/me/.ssh/id_rsa".to_string(),
                },
                stamp(None, typed),
            )
            .expect("the turn is taken")
    };
    send(true);
    let _ = started(&provider);
    send(false);
    let _ = started(&provider);
    provider
        .send_turn(SendTurnRequest {
            session_id: session.clone(),
            text: "ask hold; end".to_string(),
        })
        .expect("the turn is taken");
    let turn = started(&provider);
    let steer = |id: &str, typed: bool| {
        provider
            .steer_turn_stamped(
                SteerTurnRequest {
                    session_id: session.clone(),
                    expected_turn_id: turn.clone(),
                    client_message_id: id.to_string(),
                    text: "@a.txt".to_string(),
                    as_typed: "@a.txt".to_string(),
                },
                stamp(None, typed),
            )
            .expect("the steer is taken")
    };
    steer("eitri-1", true);
    let _ = sends(&scratch, 4);
    steer("eitri-2", false);
    assert_eq!(
        sends(&scratch, 5),
        [
            r#"send - true "end; @/home/me/.ssh/id_rsa""#,
            r#"send - false "end; \u{2060}@/home/me/.ssh/id_rsa""#,
            "send - false",
            r#"steer eitri-1 true "@a.txt""#,
            r#"steer eitri-2 false "\u{2060}@a.txt""#,
        ]
    );
}
