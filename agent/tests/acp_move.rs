//! An ACP chat moving to another process mid-turn: `AcpProvider::freeze_for_move` hands over the provider's
//! state as data and the agent's stdio, and `AcpProvider::adopt` takes them up and binds the prompt's answer
//! again by its request id. The agent is the scripted stand-in (`eitri-agent-standin`) behind a shell that
//! copies every line written to its stdin into a log, so the tests can tell that no answer reached it twice.
//! Both providers live in this one process; the records cross as JSON, as they do between two windows.

#![cfg(target_os = "linux")]

use agent::jsonrpc_link::LinkHandoff;
use agent::providers::acp::{AcpAgentSpec, AcpProvider, AcpProviderHandoff};
use agent::{
    AgentDomainEvent, AgentProvider, CloseSessionRequest, ContentKind, DirTrust, ExternalProvider, ExternalStart,
    MoveRefusedWhy, PermissionDecision, PermissionOutcome, ResolvePermissionRequest, Reviewer, SendTurnRequest,
    TrustPlan, TurnOutcome, TurnPolicy,
};
use serde_json::{json, Value};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------- the stand-in and its log

/// A directory of the test's own under the build's scratch area, emptied first.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("acp-move").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the scratch directory");
    dir
}

/// A shell that copies its stdin into `log` on the way to the stand-in, with the script in a file.
fn logged_standin(dir: &Path, script: Vec<Value>) -> Vec<OsString> {
    let wrapper = dir.join("agent.sh");
    let log = dir.join("stdin.log");
    let body = format!(
        "#!/bin/sh\ntee -a '{}' | exec '{}' \"$@\"\n",
        log.display(),
        env!("CARGO_BIN_EXE_eitri-agent-standin")
    );
    write_script(&wrapper, &body);
    let script_file = dir.join("script.json");
    std::fs::write(&script_file, Value::Array(script).to_string()).expect("the script");
    vec![
        OsString::from(wrapper),
        OsString::from(format!("@{}", script_file.display())),
    ]
}

/// Writes an executable script through a child, never from this process: a write descriptor held here
/// would be copied into any child another test thread forks meanwhile, and until that child `exec`s,
/// running the script fails with `ETXTBSY` ("Text file busy").
fn write_script(path: &Path, body: &str) {
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(r#"printf '%s' "$1" > "$2" && chmod 755 "$2""#)
        .arg("sh")
        .arg(body)
        .arg(path)
        .status()
        .expect("sh runs");
    assert!(status.success(), "writing the script failed: {status}");
}

fn stdin_lines(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("stdin.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn count_containing(lines: &[String], needle: &str) -> usize {
    lines.iter().filter(|line| line.contains(needle)).count()
}

// ---------------------------------------------------------------- script building

fn exact(text: &str) -> String {
    let mut out = String::from("^");
    for c in text.chars() {
        if "\\.+*?()|[]{}^$#&-~".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('$');
    out
}

fn expect_exact(line: &str) -> Value {
    json!({ "expect": exact(line) })
}

fn expect_request(id: u64, method: &str, params: Value) -> Value {
    expect_exact(&format!(
        r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{params}}}"#
    ))
}

fn expect_answer(id: u64, result: Value) -> Value {
    expect_exact(&format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{result}}}"#))
}

fn write(message: Value) -> Value {
    json!({ "write": message.to_string() })
}

fn reply(id: u64, result: Value) -> Value {
    write(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

fn chunk(text: &str) -> Value {
    write(
        json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1", "update":
        {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}}}),
    )
}

/// `initialize` and `session/new` answered, then the prompt (id 3) expected, then `rest`.
fn prompted(rest: Vec<Value>) -> Vec<Value> {
    let mut script = vec![
        json!({"expect": r#"^\{"jsonrpc":"2\.0","id":1,"method":"initialize""#}),
        reply(
            1,
            json!({
                "protocolVersion": 1,
                "agentInfo": {"name": "stand-in", "title": "Stand-in", "version": "1.2.3"},
                "agentCapabilities": {"loadSession": false, "sessionCapabilities": {"close": {}}},
                "authMethods": []
            }),
        ),
        expect_request(2, "session/new", json!({"cwd": "/work", "mcpServers": []})),
        reply(
            2,
            json!({"sessionId": "s1", "modes": {"currentModeId": "agent", "availableModes": []},
                        "models": {"currentModelId": "m-1"}}),
        ),
        expect_request(
            3,
            "session/prompt",
            json!({"sessionId": "s1", "prompt": [{"type": "text", "text": "go"}]}),
        ),
    ];
    script.extend(rest);
    script
}

/// A provider with its session started and its turn sent, the agent logging its stdin into `dir`.
fn running(dir: &Path, rest: Vec<Value>) -> AcpProvider {
    running_as(logged_standin(dir, prompted(rest)))
}

/// A provider with its session started and its turn sent, the agent run as `argv`.
fn running_as(argv: Vec<OsString>) -> AcpProvider {
    let spec = AcpAgentSpec {
        name: "stand-in".to_string(),
        argv,
        auto_mode: None,
    };
    let provider = AcpProvider::connect(spec, std::env::temp_dir()).expect("connects");
    provider
        .start(ExternalStart {
            cwd: "/work".to_string(),
            plan: TrustPlan {
                dirs: vec![(PathBuf::from("/work"), DirTrust::Trusted)],
                skills_off: false,
                fingerprint: "fp".to_string(),
                project_only: Vec::new(),
            },
            policy: TurnPolicy {
                reviewer: Reviewer::Agent,
            },
            resume: None,
        })
        .expect("starts");
    provider
        .send_turn_with(
            SendTurnRequest {
                session_id: "s1".to_string(),
                text: "go".to_string(),
            },
            TurnPolicy {
                reviewer: Reviewer::Agent,
            },
        )
        .expect("sends");
    provider
}

/// Freezes `source`, carries both records through JSON, and adopts and commits them in a new provider.
fn moved(source: &AcpProvider) -> AcpProvider {
    let (handoff, link, fds) = source.freeze_for_move().expect("the provider freezes");
    let handoff: AcpProviderHandoff =
        serde_json::from_str(&serde_json::to_string(&handoff).expect("the provider's record is data"))
            .expect("the provider's record reads back");
    let link: LinkHandoff = serde_json::from_str(&serde_json::to_string(&link).expect("the link's record is data"))
        .expect("the link's record reads back");
    let destination = AcpProvider::adopt(handoff, link, fds).expect("the destination adopts");
    assert!(
        destination.pump().is_empty(),
        "an adopted chat reports nothing before its commit"
    );
    assert!(
        matches!(destination.freeze_for_move().map(|_| ()), Err(MoveRefusedWhy::Link(_))),
        "an adopted chat does not move on before its own move is committed"
    );
    destination.commit().expect("the adopted link starts");
    destination
}

fn pump_until(provider: &AcpProvider, mut done: impl FnMut(&[AgentDomainEvent]) -> bool) -> Vec<AgentDomainEvent> {
    let end = Instant::now() + Duration::from_secs(30);
    let mut seen = Vec::new();
    loop {
        seen.extend(provider.pump());
        if done(&seen) {
            return seen;
        }
        assert!(Instant::now() < end, "timed out; {} events so far", seen.len());
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn completed(events: &[AgentDomainEvent]) -> Option<TurnOutcome> {
    events.iter().find_map(|e| match e {
        AgentDomainEvent::TurnCompleted { outcome, .. } => Some(*outcome),
        _ => None,
    })
}

fn texts(events: &[AgentDomainEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            AgentDomainEvent::ContentDelta {
                kind: ContentKind::Text,
                text,
                ..
            } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn close(provider: &AcpProvider) {
    provider
        .close_session(CloseSessionRequest {
            session_id: "s1".to_string(),
        })
        .expect("closes");
}

// ---------------------------------------------------------------- the tests

#[test]
fn an_acp_turn_with_a_permission_pending_completes_after_adoption() {
    let dir = scratch("permission");
    let source = running(
        &dir,
        vec![
            chunk("before "),
            write(
                json!({"jsonrpc": "2.0", "id": 7, "method": "session/request_permission", "params": {
                "sessionId": "s1",
                "toolCall": {"toolCallId": "c1", "title": "Run command", "kind": "execute", "status": "pending",
                             "rawInput": {"command": "touch x", "cwd": "/work"}},
                "options": [{"optionId": "a-1", "name": "Yes", "kind": "allow_once"},
                            {"optionId": "r-1", "name": "No", "kind": "reject_once"}]}}),
            ),
            expect_answer(7, json!({"outcome": {"outcome": "selected", "optionId": "a-1"}})),
            chunk("after"),
            reply(3, json!({"stopReason": "end_turn"})),
        ],
    );
    let before = pump_until(&source, |events| {
        events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::PermissionRequested { .. }))
    });
    let card = before
        .iter()
        .find_map(|e| match e {
            AgentDomainEvent::PermissionRequested { permission_id, .. } => Some(permission_id.clone()),
            _ => None,
        })
        .expect("a card");
    let pid = source.pid();

    let destination = moved(&source);
    assert_eq!(destination.pid(), pid, "the same agent, not a new one");
    assert!(source.pump().is_empty(), "the frozen source folds nothing more");
    destination
        .resolve_permission(ResolvePermissionRequest {
            session_id: "s1".to_string(),
            permission_id: card.clone(),
            decision: PermissionDecision::Allow,
        })
        .expect("the card waits in the destination under the number it had");
    let after = pump_until(&destination, |events| completed(events).is_some());
    assert_eq!(completed(&after), Some(TurnOutcome::Completed));
    assert!(after.iter().any(|e| matches!(
        e,
        AgentDomainEvent::PermissionResolved { permission_id, outcome: PermissionOutcome::Allowed } if *permission_id == card
    )));
    assert_eq!(texts(&before) + &texts(&after), "before after");
    close(&destination);

    let lines = stdin_lines(&dir);
    assert_eq!(
        count_containing(&lines, r#""id":7,"result""#),
        1,
        "the permission is answered once: {lines:#?}"
    );
    assert_eq!(
        count_containing(&lines, r#""method":"session/prompt""#),
        1,
        "{lines:#?}"
    );
}

#[test]
fn a_flooding_agent_with_a_full_queue_moves_with_its_messages_in_order() {
    let dir = scratch("flood");
    // More than the link's incoming queue holds, so its reader waits holding one.
    const CHUNKS: usize = 5000;
    let mut rest: Vec<Value> = (0..CHUNKS).map(|i| chunk(&format!("{i},"))).collect();
    rest.push(reply(3, json!({"stopReason": "end_turn"})));
    let source = running(&dir, rest);
    // Nothing is pumped: the agent fills the queue and the reader blocks on it.
    std::thread::sleep(Duration::from_millis(1500));
    let destination = moved(&source);
    let after = pump_until(&destination, |events| completed(events).is_some());
    let expected: String = (0..CHUNKS).map(|i| format!("{i},")).collect();
    let got = texts(&after);
    assert!(
        got == expected,
        "every message arrives once, in order ({} of {} bytes)",
        got.len(),
        expected.len()
    );
    assert_eq!(completed(&after), Some(TurnOutcome::Completed));
    close(&destination);
}

#[test]
fn an_agent_whose_end_still_waits_for_its_stderr_refuses_the_move_and_keeps_its_notice() {
    let dir = scratch("closing-notice");
    let script_file = dir.join("script.json");
    let script = prompted(vec![
        write(
            json!({"jsonrpc": "2.0", "id": 7, "method": "session/request_permission", "params": {
            "sessionId": "s1",
            "toolCall": {"toolCallId": "c1", "title": "Run command", "kind": "execute", "status": "pending",
                         "rawInput": {"command": "touch x", "cwd": "/work"}},
            "options": [{"optionId": "a-1", "name": "Yes", "kind": "allow_once"},
                        {"optionId": "r-1", "name": "No", "kind": "reject_once"}]}}),
        ),
        json!({"exit": 0}),
    ]);
    std::fs::write(&script_file, Value::Array(script).to_string()).expect("the script");
    let gate = dir.join("gate");
    // The stand-in ends, the shell closes its own copy of stdout and keeps stderr open and empty until the
    // gate file appears, capped so a failed test leaves no child behind: the end of the output is read while
    // the reason is still to come.
    let wrapper = dir.join("agent.sh");
    write_script(
        &wrapper,
        &format!(
            "#!/bin/sh\n'{}' '@{}'\nexec >&-\ni=0\nwhile [ ! -e '{}' ] && [ \"$i\" -lt 1000 ]; do sleep 0.01; i=$((i+1)); done\necho 'stand-in: gone' >&2\n",
            env!("CARGO_BIN_EXE_eitri-agent-standin"),
            script_file.display(),
            gate.display()
        ),
    );
    let provider = running_as(vec![OsString::from(&wrapper)]);
    let before = pump_until(&provider, |events| {
        events.iter().any(|e| {
            matches!(
                e,
                AgentDomainEvent::PermissionResolved {
                    outcome: PermissionOutcome::ProviderFailed,
                    ..
                }
            )
        })
    });
    assert!(
        !before
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::SessionUnavailable { .. })),
        "the notice waits for stderr: {before:#?}"
    );

    // The agent is gone though its notice is not out yet: there is nothing to move, and the refusal leaves the
    // notice where the pump finishes it.
    let refused = provider
        .freeze_for_move()
        .map(|_| ())
        .expect_err("an ended agent does not move");
    assert_eq!(refused, MoveRefusedWhy::Ended);

    std::fs::write(&gate, b"").expect("the gate opens");
    let after = pump_until(&provider, |events| {
        events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::SessionUnavailable { .. }))
    });
    let notices: Vec<&String> = after
        .iter()
        .filter_map(|e| match e {
            AgentDomainEvent::SessionUnavailable { reason } => Some(reason),
            _ => None,
        })
        .collect();
    assert_eq!(notices.len(), 1, "{after:#?}");
    assert!(notices[0].contains("stand-in: gone"), "{}", notices[0]);
}

#[test]
fn a_saved_session_still_replaying_refuses_the_move_at_once() {
    let dir = scratch("replaying");
    let script = vec![
        json!({"expect": r#"^\{"jsonrpc":"2\.0","id":1,"method":"initialize""#}),
        reply(
            1,
            json!({
                "protocolVersion": 1,
                "agentInfo": {"name": "stand-in", "title": "Stand-in", "version": "1.2.3"},
                "agentCapabilities": {"loadSession": true, "sessionCapabilities": {"close": {}}},
                "authMethods": []
            }),
        ),
        json!({"expect": r#"^\{"jsonrpc":"2\.0","id":2,"method":"session/load""#}),
        // A long replay: the answer comes well after a start's short wait.
        json!({"sleep_ms": 3000}),
        reply(
            2,
            json!({"modes": {"currentModeId": "agent", "availableModes": []}, "models": {"currentModelId": "m-1"}}),
        ),
    ];
    let spec = AcpAgentSpec {
        name: "stand-in".to_string(),
        argv: logged_standin(&dir, script),
        auto_mode: None,
    };
    let provider = AcpProvider::connect(spec, std::env::temp_dir()).expect("connects");
    std::thread::scope(|scope| {
        let loading = scope.spawn(|| {
            provider.start(ExternalStart {
                cwd: "/work".to_string(),
                plan: TrustPlan {
                    dirs: vec![(PathBuf::from("/work"), DirTrust::Trusted)],
                    skills_off: false,
                    fingerprint: "fp".to_string(),
                    project_only: Vec::new(),
                },
                policy: TurnPolicy {
                    reviewer: Reviewer::Agent,
                },
                resume: Some("s1".to_string()),
            })
        });
        let end = Instant::now() + Duration::from_secs(10);
        while count_containing(&stdin_lines(&dir), "session/load") == 0 {
            assert!(Instant::now() < end, "the load was never sent");
            std::thread::sleep(Duration::from_millis(10));
        }
        let asked = Instant::now();
        let refused = provider
            .freeze_for_move()
            .map(|_| ())
            .expect_err("a replay holds the move");
        assert_eq!(refused, MoveRefusedWhy::HistoryLoading);
        assert!(
            asked.elapsed() < Duration::from_secs(1),
            "a replay is refused at once, not after the start's wait"
        );
        assert_eq!(
            loading.join().expect("the start").expect("the saved session opens"),
            "s1",
            "the refused freeze took nothing from the load"
        );
    });
    close(&provider);
}

#[test]
fn an_acp_tab_moves_mid_turn() {
    let dir = scratch("by-kind");
    let source = running(
        &dir,
        vec![
            chunk("before "),
            write(
                json!({"jsonrpc": "2.0", "id": 7, "method": "session/request_permission", "params": {
                "sessionId": "s1",
                "toolCall": {"toolCallId": "c1", "title": "Run command", "kind": "execute", "status": "pending",
                             "rawInput": {"command": "touch x", "cwd": "/work"}},
                "options": [{"optionId": "a-1", "name": "Yes", "kind": "allow_once"},
                            {"optionId": "r-1", "name": "No", "kind": "reject_once"}]}}),
            ),
            expect_answer(7, json!({"outcome": {"outcome": "selected", "optionId": "a-1"}})),
            chunk("after"),
            reply(3, json!({"stopReason": "end_turn"})),
        ],
    );
    let before = pump_until(&source, |events| {
        events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::PermissionRequested { .. }))
    });
    let card = before
        .iter()
        .find_map(|e| match e {
            AgentDomainEvent::PermissionRequested { permission_id, .. } => Some(permission_id.clone()),
            _ => None,
        })
        .expect("a card");

    // As two windows move it: through the face every provider moves by, the record as JSON, copies of the
    // descriptors, taken up again by the kind the record names.
    let mover = AgentProvider::mover(&source).expect("an ACP chat moves");
    assert!(mover.live_move(), "its running turn moves with it");
    let frozen = mover.freeze_for_chat_move().expect("the provider freezes");
    let record: Value = serde_json::from_str(&frozen.record.to_string()).expect("the record reads back");
    let kind = agent::chat_mover::MovedKind::from_wire(frozen.kind.wire()).expect("a kind this build moves");
    let adopted = agent::chat_mover::adopt(kind, &record, frozen.copies().unwrap()).expect("taken up");
    let destination = adopted.agent;
    assert!(adopted.external.is_some(), "an ACP chat has its external side");
    destination.mover().unwrap().commit_chat_move().expect("committed");
    mover.forget_chat_move();
    drop(frozen);

    destination
        .resolve_permission(ResolvePermissionRequest {
            session_id: "s1".to_string(),
            permission_id: card.clone(),
            decision: PermissionDecision::Allow,
        })
        .expect("the card waits in the destination under the number it had");
    let end = Instant::now() + Duration::from_secs(30);
    let mut after = Vec::new();
    while completed(&after).is_none() {
        assert!(Instant::now() < end, "timed out; {} events so far", after.len());
        after.extend(destination.pump());
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(completed(&after), Some(TurnOutcome::Completed));
    assert_eq!(texts(&before) + &texts(&after), "before after");
    destination
        .close_session(CloseSessionRequest {
            session_id: "s1".to_string(),
        })
        .expect("closes");
    let lines = stdin_lines(&dir);
    assert_eq!(
        count_containing(&lines, r#""id":7,"result""#),
        1,
        "the permission is answered once: {lines:#?}"
    );
}
