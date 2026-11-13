//! A Codex chat moving to another process mid-turn: `CodexProvider::freeze_for_move` hands over the
//! provider's state as data and the agent's stdio, and `CodexProvider::adopt` takes them up and binds every
//! waiting answer again by its request id. The agent is the scripted stand-in (`eitri-agent-standin`)
//! behind a shell that copies every line written to its stdin into a log, so the tests can tell that no
//! answer reached it twice. Both providers live in this one process; the records cross as JSON, as they do
//! between two windows.

#![cfg(target_os = "linux")]

use agent::jsonrpc_link::LinkHandoff;
use agent::providers::codex::{CodexLaunch, CodexProvider, CodexProviderHandoff};
use agent::{
    AgentDomainEvent, AgentProvider, CloseSessionRequest, DirTrust, ExternalProvider, ExternalStart,
    InterruptTurnRequest, MoveRefusedWhy, PermissionDecision, PermissionOutcome, ResolvePermissionRequest, Reviewer,
    SendTurnRequest, SteerTurnRequest, TrustPlan, TurnOutcome, TurnPolicy,
};
use serde_json::{json, Value};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[path = "support/script_gate.rs"]
mod script_gate;
use script_gate::{until, ScriptGate};

const UA: &str = "eitri-probe/0.161.0 (Arch Linux Rolling Release; x86_64) tmux-256color (eitri-probe; 0)";

// ---------------------------------------------------------------- the stand-in and its log

/// A directory of the test's own under the build's scratch area, emptied first.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("codex-move").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the scratch directory");
    dir
}

/// A shell that runs `program` with its arguments, copying every line written to its stdin into `dir`'s
/// log on the way.
fn logged_program(dir: &Path, program: &Path) -> PathBuf {
    let wrapper = dir.join("agent.sh");
    let log = dir.join("stdin.log");
    let body = format!(
        "#!/bin/sh\ntee -a '{}' | exec '{}' \"$@\"\n",
        log.display(),
        program.display()
    );
    write_script(&wrapper, &body);
    wrapper
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

/// The stand-in behind [`logged_program`], with the script in a file.
fn logged_standin(dir: &Path, script: Vec<Value>) -> (PathBuf, OsString) {
    let wrapper = logged_program(dir, Path::new(env!("CARGO_BIN_EXE_eitri-agent-standin")));
    let script_file = dir.join("script.json");
    std::fs::write(&script_file, Value::Array(script).to_string()).expect("the script");
    (wrapper, OsString::from(format!("@{}", script_file.display())))
}

/// Every line written to the agent's stdin, by either provider.
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

/// The ids of the answers among `lines`, in the order they were written.
fn answer_ids(lines: &[String]) -> Vec<Value> {
    lines
        .iter()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|message| message.get("method").is_none() && message.get("id").is_some())
        .map(|message| message["id"].clone())
        .collect()
}

fn no_answer_twice(lines: &[String]) {
    let ids = answer_ids(lines);
    for (at, id) in ids.iter().enumerate() {
        assert!(
            !ids[..at].contains(id),
            "the answer to {id} was written twice: {lines:#?}"
        );
    }
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

fn expect_request(id: u64, method: &str) -> Value {
    json!({ "expect": format!(r#"^\{{"id":{id},"method":"{method}""#) })
}

fn expect_answer(id: Value, result: &str) -> Value {
    expect_exact(&format!(r#"{{"id":{id},"result":{result}}}"#))
}

fn write(message: Value) -> Value {
    json!({ "write": message.to_string() })
}

fn item_started(thread: &str, turn: &str, item: Value) -> Value {
    write(json!({"method": "item/started", "params": {"threadId": thread, "turnId": turn, "item": item}}))
}

fn item_completed(thread: &str, turn: &str, item: Value) -> Value {
    write(json!({"method": "item/completed", "params": {"threadId": thread, "turnId": turn, "item": item}}))
}

fn turn_completed(thread: &str, turn: &str, status: &str) -> Value {
    write(json!({"method": "turn/completed", "params": {
        "threadId": thread,
        "turn": {"id": turn, "items": [], "itemsView": "summary", "status": status, "error": null},
    }}))
}

fn command_item(id: &str, status: &str) -> Value {
    json!({"type": "commandExecution", "id": id, "command": "touch x", "cwd": "/work/proj", "status": status})
}

/// The handshake and `thread/start` (id 3) answered, then `rest`.
fn session_script(thread: &str, rest: Vec<Value>) -> Vec<Value> {
    let mut script = vec![
        expect_request(1, "initialize"),
        write(json!({"id": 1, "result": {"userAgent": UA, "codexHome": "/x"}})),
        expect_exact(r#"{"method":"initialized"}"#),
        expect_request(2, "account/read"),
        write(json!({"id": 2, "result": {"account": {"type": "apiKey"}, "requiresOpenaiAuth": true}})),
        expect_request(3, "thread/start"),
        write(json!({"id": 3, "result": {
            "thread": {"id": thread, "cwd": "/work/proj"}, "model": "gpt-test", "cwd": "/work/proj",
            "approvalPolicy": "on-request", "approvalsReviewer": "user",
            "sandbox": {"type": "workspaceWrite", "writableRoots": [], "networkAccess": false,
                        "excludeTmpdirEnvVar": false, "excludeSlashTmp": false},
        }})),
    ];
    script.extend(rest);
    script
}

/// `turn/start` (id 4) answered with `turn`.
fn turn_start(turn: &str) -> Vec<Value> {
    vec![
        expect_request(4, "turn/start"),
        write(json!({"id": 4, "result": {"turn": {
            "id": turn, "items": [], "itemsView": "notLoaded", "status": "inProgress", "error": null,
        }}})),
    ]
}

/// A provider with its session started, the agent logging its stdin into `dir`.
fn started(dir: &Path, thread: &str, rest: Vec<Value>) -> CodexProvider {
    let (program, script) = logged_standin(dir, session_script(thread, rest));
    let launch = CodexLaunch {
        program,
        cwd: std::env::temp_dir(),
        state_home: dir.to_path_buf(),
    };
    let provider = CodexProvider::connect_with_args(launch, vec![script]).expect("the handshake passes");
    let id = provider
        .start(ExternalStart {
            cwd: "/work/proj".into(),
            plan: TrustPlan {
                dirs: vec![(PathBuf::from("/work/proj"), DirTrust::Untrusted)],
                skills_off: true,
                fingerprint: "fp".into(),
                project_only: Vec::new(),
            },
            policy: TurnPolicy {
                reviewer: Reviewer::User,
                sandbox: agent::Sandbox::Project,
            },
            resume: None,
            editor_server: None,
        })
        .expect("the session starts");
    assert_eq!(id, thread);
    provider
}

fn send(provider: &CodexProvider, thread: &str) -> String {
    provider
        .send_turn_with(
            SendTurnRequest::text(thread.to_string(), "go"),
            TurnPolicy {
                reviewer: Reviewer::User,
                sandbox: agent::Sandbox::Project,
            },
        )
        .expect("the turn starts")
}

/// A provider with its session started and one turn running, the agent logging its stdin into `dir`.
fn running(dir: &Path, thread: &str, turn: &str, rest: Vec<Value>) -> CodexProvider {
    let mut steps = turn_start(turn);
    steps.extend(rest);
    let provider = started(dir, thread, steps);
    assert_eq!(send(&provider, thread), turn);
    provider
}

// ---------------------------------------------------------------- the move

/// Freezes `source`, carries both records through JSON, and adopts and commits them in a new provider.
fn moved(source: &CodexProvider) -> CodexProvider {
    let (handoff, link, fds) = source
        .freeze_with_clock_for_tests(Instant::now, |_| panic!("a settled provider must not wait for a start"))
        .expect("the provider freezes");
    let handoff: CodexProviderHandoff =
        serde_json::from_str(&serde_json::to_string(&handoff).expect("the provider's record is data"))
            .expect("the provider's record reads back");
    let link: LinkHandoff = serde_json::from_str(&serde_json::to_string(&link).expect("the link's record is data"))
        .expect("the link's record reads back");
    let destination = CodexProvider::adopt(handoff, link, fds).expect("the destination adopts");
    before_commit(&destination);
    destination.commit().expect("the adopted link starts");
    destination
}

/// What an adopted chat does before its move is committed: it reports nothing, and moves on nowhere.
fn before_commit(destination: &CodexProvider) {
    assert!(
        destination.pump().is_empty(),
        "an adopted chat reports nothing before its commit"
    );
    assert!(
        matches!(destination.freeze_for_move().map(|_| ()), Err(MoveRefusedWhy::Link(_))),
        "an adopted chat does not move on before its own move is committed"
    );
}

fn pump_until(provider: &CodexProvider, mut done: impl FnMut(&[AgentDomainEvent]) -> bool) -> Vec<AgentDomainEvent> {
    let end = Instant::now() + Duration::from_secs(20);
    let mut seen = Vec::new();
    loop {
        seen.extend(provider.pump());
        if done(&seen) {
            return seen;
        }
        assert!(Instant::now() < end, "timed out; the events so far: {seen:#?}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn completed(events: &[AgentDomainEvent]) -> Option<TurnOutcome> {
    events.iter().find_map(|e| match e {
        AgentDomainEvent::TurnCompleted { outcome, .. } => Some(*outcome),
        _ => None,
    })
}

fn close(provider: &CodexProvider, thread: &str) {
    provider
        .close_session(CloseSessionRequest {
            session_id: thread.to_string(),
        })
        .expect("closes");
}

// ---------------------------------------------------------------- the tests

#[test]
fn a_codex_turn_with_an_approval_pending_completes_after_adoption() {
    let dir = scratch("approval");
    let (thread, turn) = ("th-m1", "tu-m1");
    let source = running(
        &dir,
        thread,
        turn,
        vec![
            item_started(thread, turn, command_item("exec-1", "inProgress")),
            write(
                json!({"id": 0, "method": "item/commandExecution/requestApproval", "params": {
                "kind": "command", "threadId": thread, "turnId": turn, "itemId": "exec-1",
                "reason": "May I?", "command": "touch x", "cwd": "/work/proj"}}),
            ),
            expect_answer(json!(0), r#"{"decision":"accept"}"#),
            item_completed(thread, turn, command_item("exec-1", "completed")),
            turn_completed(thread, turn, "completed"),
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
            session_id: thread.to_string(),
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
    close(&destination, thread);

    let lines = stdin_lines(&dir);
    assert_eq!(
        count_containing(&lines, r#"{"id":0,"result""#),
        1,
        "the approval is answered once: {lines:#?}"
    );
    no_answer_twice(&lines);
}

#[test]
fn an_interrupt_sent_just_before_the_freeze_resolves_after_adoption() {
    let gate = ScriptGate::new();
    let dir = scratch("interrupt");
    let (thread, turn) = ("th-m2", "tu-m2");
    let source = running(
        &dir,
        thread,
        turn,
        vec![
            expect_request(5, "turn/interrupt"),
            // The refusal comes once the source has stopped reading.
            gate.step(),
            write(json!({"id": 5, "error": {"code": -32600, "message": "no active turn to interrupt"}})),
            // Only a provider that read the refusal sends the interrupt again, under the next id.
            expect_request(6, "turn/interrupt"),
            write(json!({"id": 6, "result": {}})),
            turn_completed(thread, turn, "interrupted"),
        ],
    );
    pump_until(&source, |events| {
        events.iter().any(|e| matches!(e, AgentDomainEvent::TurnStarted { .. }))
    });
    source
        .interrupt_turn(InterruptTurnRequest {
            session_id: thread.to_string(),
        })
        .expect("the interrupt is sent");
    gate.wait();
    let destination = moved(&source);
    destination.pause_time_for_tests();
    gate.release();
    until(|| destination.lines_read_for_tests() >= 1);
    let mut after = destination.pump();
    destination.advance_time_for_tests(Duration::from_millis(100));
    after.extend(pump_until(&destination, |events| completed(events).is_some()));
    assert_eq!(completed(&after), Some(TurnOutcome::Interrupted));
    close(&destination, thread);

    let lines = stdin_lines(&dir);
    assert_eq!(
        count_containing(&lines, r#"{"id":5,"method":"turn/interrupt""#),
        1,
        "{lines:#?}"
    );
    assert_eq!(
        count_containing(&lines, r#"{"id":6,"method":"turn/interrupt""#),
        1,
        "{lines:#?}"
    );
}

#[test]
fn a_steer_answer_read_after_the_last_pump_reaches_the_destination_once() {
    let answer = ScriptGate::new();
    let end = ScriptGate::new();
    let dir = scratch("steer");
    let (thread, turn) = ("th-m3", "tu-m3");
    let source = running(
        &dir,
        thread,
        turn,
        vec![
            expect_request(5, "turn/steer"),
            // After the source's last pump, before its link stops.
            answer.step(),
            write(json!({"id": 5, "error": {"code": -32000, "message": "not now"}})),
            // Only once the move is over.
            end.step(),
            turn_completed(thread, turn, "completed"),
        ],
    );
    pump_until(&source, |events| {
        events.iter().any(|e| matches!(e, AgentDomainEvent::TurnStarted { .. }))
    });
    source
        .steer_turn(SteerTurnRequest {
            session_id: thread.to_string(),
            expected_turn_id: turn.to_string(),
            client_message_id: "m1".to_string(),
            text: "and this".to_string(),
            as_typed: "and this".to_string(),
        })
        .expect("the message is sent");
    let last_pump = source.pump();
    // The answer is read off the pipe into the source's waiting slot, and nobody looks at it there.
    answer.wait();
    answer.release();
    end.wait();
    until(|| source.lines_read_for_tests() >= 5);
    let destination = moved(&source);
    end.release();
    let after = pump_until(&destination, |events| completed(events).is_some());
    let dropped = |events: &[AgentDomainEvent]| {
        events
            .iter()
            .filter(|e| matches!(e, AgentDomainEvent::UserMessageDropped { client_message_id, .. } if client_message_id == "m1"))
            .count()
    };
    assert_eq!(dropped(&last_pump), 0, "the source never read the answer");
    assert_eq!(
        dropped(&after),
        1,
        "the destination reports what became of the message once: {after:#?}"
    );
    close(&destination, thread);

    let lines = stdin_lines(&dir);
    assert_eq!(
        count_containing(&lines, r#""method":"turn/steer""#),
        1,
        "no second steer: {lines:#?}"
    );
}

#[test]
fn an_interrupt_deadline_runs_out_in_the_destination_only_after_the_commit() {
    let gate = ScriptGate::new();
    let dir = scratch("deadline");
    let (thread, turn) = ("th-m9", "tu-m9");
    let source = running(
        &dir,
        thread,
        turn,
        vec![
            expect_request(5, "turn/interrupt"),
            write(json!({"id": 5, "result": {}})),
            // The turn ends later than the interrupt's grace counted from the interrupt, but within it
            // counted from the commit.
            gate.step(),
            turn_completed(thread, turn, "interrupted"),
        ],
    );
    pump_until(&source, |events| {
        events.iter().any(|e| matches!(e, AgentDomainEvent::TurnStarted { .. }))
    });
    let interrupted_at = source.pause_time_for_tests();
    source
        .interrupt_turn(InterruptTurnRequest {
            session_id: thread.to_string(),
        })
        .expect("the interrupt is sent");
    let remaining = source
        .interrupt_deadline_for_tests()
        .unwrap()
        .duration_since(interrupted_at);
    let (handoff, link, fds) = source
        .freeze_with_clock_for_tests(Instant::now, |_| panic!("a settled provider must not wait for a start"))
        .expect("the provider freezes");
    let destination = CodexProvider::adopt(handoff, link, fds).expect("the destination adopts");
    // Pumped past the grace before the commit: nothing is read meanwhile, so nothing may run out.
    let paused = destination.pause_time_for_tests();
    let parked_for = agent::providers::codex::provider::INTERRUPT_SETTLE + Duration::from_millis(500);
    destination.advance_time_for_tests(parked_for);
    before_commit(&destination);
    destination.commit().expect("the adopted link starts");
    gate.wait();
    assert_eq!(
        destination
            .interrupt_deadline_for_tests()
            .unwrap()
            .checked_duration_since(paused + parked_for),
        Some(remaining),
        "commit rebases the live deadline before the child can finish the turn"
    );
    let gated = destination.pump();
    assert!(
        !gated.iter().any(|e| matches!(
            e,
            AgentDomainEvent::SessionUnavailable { .. } | AgentDomainEvent::TurnCompleted { .. }
        )),
        "the rebased interrupt deadline is still live while turn completion is held: {gated:#?}"
    );
    assert!(
        !destination.closed_for_tests(),
        "the rebased deadline did not stop the session"
    );
    gate.release();
    let after = pump_until(&destination, |events| completed(events).is_some());
    assert_eq!(completed(&after), Some(TurnOutcome::Interrupted), "{after:#?}");
    assert!(
        !after
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::SessionUnavailable { .. })),
        "the session goes on: {after:#?}"
    );
    close(&destination, thread);
    let lines = stdin_lines(&dir);
    assert_eq!(
        count_containing(&lines, r#""method":"turn/interrupt""#),
        1,
        "{lines:#?}"
    );
}

#[test]
fn a_provider_that_is_stopping_refuses_the_move() {
    let dir = scratch("stopping");
    let (thread, turn) = ("th-m4", "tu-m4");
    let source = running(
        &dir,
        thread,
        turn,
        vec![
            item_started(thread, turn, command_item("exec-bg", "inProgress")),
            turn_completed(thread, turn, "completed"),
        ],
    );
    let events = pump_until(&source, |events| completed(events).is_some());
    let shown = events
        .iter()
        .rev()
        .find_map(|e| match e {
            AgentDomainEvent::BackgroundCalls { running } => Some(*running),
            _ => None,
        })
        .unwrap_or(0);
    assert!(
        shown > 0,
        "the command the turn left running counts as background: {events:#?}"
    );
    source
        .stop_background(thread, shown, &[])
        .expect("the background is stopped");
    let refused = source
        .freeze_with_clock_for_tests(Instant::now, |_| panic!("this refusal must not wait for a start"))
        .map(|_| ())
        .expect_err("a stopping provider does not freeze");
    assert_eq!(refused, MoveRefusedWhy::Stopping);
    // Refused, the provider goes on as it was: its stop ends the session.
    let after = pump_until(&source, |events| {
        events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::SessionUnavailable { .. }))
    });
    assert!(!after.is_empty());
}

#[test]
fn a_turn_start_unanswered_for_two_seconds_refuses_the_move_and_the_turn_goes_on() {
    let gate = ScriptGate::new();
    let dir = scratch("starting");
    let (thread, turn) = ("th-m5", "tu-m5");
    let mut rest = vec![expect_request(4, "turn/start"), gate.step()];
    rest.push(write(json!({"id": 4, "result": {"turn": {
        "id": turn, "items": [], "itemsView": "notLoaded", "status": "inProgress", "error": null,
    }}})));
    rest.push(turn_completed(thread, turn, "completed"));
    let source = Arc::new(started(&dir, thread, rest));
    let sender = {
        let source = Arc::clone(&source);
        std::thread::spawn(move || send(&source, thread))
    };
    gate.wait();
    let start = Instant::now();
    let clock = std::cell::Cell::new(start);
    let pauses = std::cell::Cell::new(0);
    let refused = source
        .freeze_with_clock_for_tests(
            || clock.get(),
            |by| {
                pauses.set(pauses.get() + 1);
                assert!(pauses.get() <= 201, "start wait ignored its deadline");
                clock.set(clock.get() + by);
            },
        )
        .map(|_| ())
        .expect_err("a turn still starting holds the move");
    assert_eq!(refused, MoveRefusedWhy::Starting);
    assert_eq!(clock.get() - start, Duration::from_secs(2));
    gate.release();
    assert_eq!(
        sender.join().expect("the sender"),
        turn,
        "the refused freeze took nothing from the turn's start"
    );
    let after = pump_until(&source, |events| completed(events).is_some());
    assert_eq!(completed(&after), Some(TurnOutcome::Completed));
    close(&source, thread);
}

#[test]
fn an_idle_tab_moves_and_its_next_turn_starts_in_the_destination() {
    let dir = scratch("idle");
    let (thread, turn) = ("th-m7", "tu-m7");
    let source = running(
        &dir,
        thread,
        turn,
        vec![
            turn_completed(thread, turn, "completed"),
            // The next turn is the destination's, under the id after the source's last.
            expect_request(5, "turn/start"),
            write(json!({"id": 5, "result": {"turn": {
                "id": "tu-m7b", "items": [], "itemsView": "notLoaded", "status": "inProgress", "error": null,
            }}})),
            turn_completed(thread, "tu-m7b", "completed"),
        ],
    );
    pump_until(&source, |events| completed(events).is_some());
    // An idle agent writes nothing more; the freeze stops its reader anyway.
    let probe = source.reader_wait_probe_for_tests();
    until(|| probe.waiting());
    let destination = moved(&source);
    assert!(
        probe.stopped_on_signal(),
        "the idle reader stops on its stop fd, without a polling deadline"
    );
    assert_eq!(send(&destination, thread), "tu-m7b");
    let after = pump_until(&destination, |events| completed(events).is_some());
    assert_eq!(completed(&after), Some(TurnOutcome::Completed));
    close(&destination, thread);
    assert_eq!(count_containing(&stdin_lines(&dir), r#""method":"turn/start""#), 2);
}

#[test]
fn a_move_given_back_before_its_commit_leaves_the_source_as_it_was() {
    let dir = scratch("given-back");
    let (thread, turn) = ("th-m8", "tu-m8");
    let source = running(
        &dir,
        thread,
        turn,
        vec![
            item_started(thread, turn, command_item("exec-1", "inProgress")),
            write(
                json!({"id": 0, "method": "item/commandExecution/requestApproval", "params": {
                "kind": "command", "threadId": thread, "turnId": turn, "itemId": "exec-1",
                "reason": "May I?", "command": "touch x", "cwd": "/work/proj"}}),
            ),
            expect_answer(json!(0), r#"{"decision":"decline"}"#),
            turn_completed(thread, turn, "completed"),
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
    let (handoff, link, fds) = source
        .freeze_with_clock_for_tests(Instant::now, |_| panic!("a settled provider must not wait for a start"))
        .expect("the provider freezes");
    // A destination that took the records up and was dropped before the commit wrote nothing.
    let (h2, l2) = (
        serde_json::to_value(&handoff).unwrap(),
        serde_json::to_value(&link).unwrap(),
    );
    let dup = |fd: &std::os::fd::OwnedFd| fd.try_clone().expect("dup");
    let copies = [dup(&fds[0]), dup(&fds[1]), dup(&fds[2])];
    drop(
        CodexProvider::adopt(
            serde_json::from_value(h2).unwrap(),
            serde_json::from_value(l2).unwrap(),
            copies,
        )
        .expect("adopts"),
    );
    source
        .resume_after_move(handoff, link, fds)
        .expect("the source takes its chat back");
    source
        .resolve_permission(ResolvePermissionRequest {
            session_id: thread.to_string(),
            permission_id: card,
            decision: PermissionDecision::Deny { reason: None },
        })
        .expect("the card still waits in the source");
    let after = pump_until(&source, |events| completed(events).is_some());
    assert_eq!(completed(&after), Some(TurnOutcome::Completed));
    close(&source, thread);
    let lines = stdin_lines(&dir);
    assert_eq!(count_containing(&lines, r#"{"id":0,"result""#), 1, "{lines:#?}");
}

#[test]
fn a_closed_session_refuses_the_move() {
    let dir = scratch("closed");
    let (thread, turn) = ("th-m6", "tu-m6");
    let source = running(&dir, thread, turn, vec![turn_completed(thread, turn, "completed")]);
    pump_until(&source, |events| completed(events).is_some());
    close(&source, thread);
    assert_eq!(
        source.freeze_for_move().map(|_| ()).expect_err("ended"),
        MoveRefusedWhy::Ended
    );
}

// ---------------------------------------------------------------- the real codex

#[path = "support/codex_peer.rs"]
mod codex_peer;

/// The real `codex app-server` behind [`logged_program`], in a private home whose model is the scripted one
/// on loopback, with no multiplexer and no key in its environment.
fn real_provider(program: &Path, s: &codex_peer::Scratch, home: &Path, project: &Path) -> CodexProvider {
    let no_proxy = OsString::from("127.0.0.1,localhost");
    CodexProvider::connect_with_env(
        CodexLaunch {
            program: program.to_path_buf(),
            cwd: project.to_path_buf(),
            state_home: s.dir("state"),
        },
        ["app-server", "--listen", "stdio://"]
            .iter()
            .map(OsString::from)
            .collect(),
        vec![
            (OsString::from("CODEX_HOME"), home.as_os_str().to_os_string()),
            (OsString::from("NO_PROXY"), no_proxy.clone()),
            (OsString::from("no_proxy"), no_proxy),
        ],
        [
            "TMUX",
            "TMUX_PANE",
            "NVIM",
            "NVIM_LISTEN_ADDRESS",
            "OPENAI_API_KEY",
            "CODEX_API_KEY",
        ]
        .iter()
        .map(OsString::from)
        .collect(),
    )
    .expect("the provider connects to the real codex")
}

#[test]
#[ignore = "runs the real codex (on PATH) against the scripted model; no login, no network"]
fn a_real_codex_turn_with_an_approval_pending_completes_after_adoption() {
    use agent::providers::codex::scripted_model::{ScriptedModel, Step};
    let codex = codex_peer::real_codex();
    let s = codex_peer::Scratch::new("move-approval");
    let outside = s.dir("outside");
    let model = ScriptedModel::start(vec![Step::exec(
        "g1",
        &format!("touch {}/one", codex_peer::text_of(&outside)),
        true,
    )
    .when_body_has("PROMPT-GATED")
    .unless_body_has("\"g1\"")])
    .expect("model");
    let home = s.dir("home");
    codex_peer::write_home(&home, &model, "");
    let project = s.dir("proj");
    codex_peer::write_project(&project, "");
    let logs = s.dir("log");
    let source = real_provider(&logged_program(&logs, &codex), &s, &home, &project);
    let policy = TurnPolicy {
        reviewer: Reviewer::User,
        sandbox: agent::Sandbox::Project,
    };
    let thread = source
        .start(ExternalStart {
            cwd: codex_peer::text_of(&project).to_string(),
            plan: codex_peer::plan(&project, DirTrust::Untrusted),
            policy,
            resume: None,
            editor_server: None,
        })
        .expect("the session starts");
    source
        .send_turn_with(
            SendTurnRequest::text(thread.clone(), "PROMPT-GATED".to_string()),
            policy,
        )
        .expect("the turn starts");
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

    let destination = moved(&source);
    destination
        .resolve_permission(ResolvePermissionRequest {
            session_id: thread.clone(),
            permission_id: card,
            decision: PermissionDecision::Allow,
        })
        .expect("the card waits in the destination under the number it had");
    let end = Instant::now() + Duration::from_secs(60);
    let mut after = Vec::new();
    while completed(&after).is_none() {
        assert!(
            Instant::now() < end,
            "the turn did not end; the events so far: {after:#?}"
        );
        after.extend(destination.pump());
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(completed(&after), Some(TurnOutcome::Completed));
    assert!(outside.join("one").exists(), "the approved command ran");
    close(&destination, &thread);
    no_answer_twice(&stdin_lines(&logs));
}

// ---------------------------------------------------------------- the move as a window makes it

/// What one window does with a chat's provider and the other window with what came: frozen through the face every
/// provider moves by, its record carried as JSON and copies of its descriptors, taken up again by the kind the
/// record names and committed, the source letting go once the other holds it.
fn moved_by_its_kind(source: &CodexProvider) -> std::sync::Arc<dyn AgentProvider + Send + Sync> {
    let mover = AgentProvider::mover(source).expect("a Codex chat moves");
    let frozen = mover.freeze_for_chat_move().expect("the provider freezes");
    let record: Value = serde_json::from_str(&frozen.record.to_string()).expect("the record reads back");
    let copies = frozen.copies().expect("copies of the descriptors");
    let kind = agent::chat_mover::MovedKind::from_wire(frozen.kind.wire()).expect("a kind this build moves");
    let adopted = agent::chat_mover::adopt(kind, &record, copies).expect("the destination takes it up");
    assert!(adopted.agent.pump().is_empty(), "nothing is reported before the commit");
    adopted
        .agent
        .mover()
        .expect("the adopted provider moves too")
        .commit_chat_move()
        .expect("the commit starts it");
    mover.forget_chat_move();
    // The source's own descriptors close: the destination holds its copies.
    drop(frozen);
    adopted.agent
}

#[test]
fn a_codex_tab_moves_mid_turn_with_its_approval_under_the_same_card_number_and_no_reply_twice() {
    let dir = scratch("approval-by-kind");
    let (thread, turn) = ("th-k1", "tu-k1");
    let source = running(
        &dir,
        thread,
        turn,
        vec![
            item_started(thread, turn, command_item("exec-1", "inProgress")),
            write(
                json!({"id": 0, "method": "item/commandExecution/requestApproval", "params": {
                "kind": "command", "threadId": thread, "turnId": turn, "itemId": "exec-1",
                "reason": "May I?", "command": "touch x", "cwd": "/work/proj"}}),
            ),
            expect_answer(json!(0), r#"{"decision":"accept"}"#),
            item_completed(thread, turn, command_item("exec-1", "completed")),
            turn_completed(thread, turn, "completed"),
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

    let destination = moved_by_its_kind(&source);
    assert!(source.pump().is_empty(), "the source folds nothing more");
    destination
        .resolve_permission(ResolvePermissionRequest {
            session_id: thread.to_string(),
            permission_id: card.clone(),
            decision: PermissionDecision::Allow,
        })
        .expect("the card waits in the destination under the number it had");
    let end = Instant::now() + Duration::from_secs(20);
    let mut after = Vec::new();
    while completed(&after).is_none() {
        assert!(Instant::now() < end, "timed out; the events so far: {after:#?}");
        after.extend(destination.pump());
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(completed(&after), Some(TurnOutcome::Completed));
    assert!(after.iter().any(|e| matches!(
        e,
        AgentDomainEvent::PermissionResolved { permission_id, outcome: PermissionOutcome::Allowed } if *permission_id == card
    )));
    destination
        .close_session(CloseSessionRequest {
            session_id: thread.to_string(),
        })
        .expect("closes");
    let lines = stdin_lines(&dir);
    assert_eq!(
        count_containing(&lines, r#"{"id":0,"result""#),
        1,
        "the approval is answered once: {lines:#?}"
    );
    no_answer_twice(&lines);
}

#[test]
fn a_record_of_another_kind_or_with_the_wrong_descriptors_is_refused_whole() {
    use agent::chat_mover::{adopt, MovedKind};
    let pipe = || -> std::os::fd::OwnedFd { std::io::pipe().unwrap().0.into() };
    let refused = |kind, record: Value, fds: Vec<std::os::fd::OwnedFd>| match adopt(kind, &record, fds) {
        Ok(_) => panic!("{record} was taken up"),
        Err(why) => why,
    };
    assert!(refused(MovedKind::Codex, json!({}), vec![pipe(), pipe()]).contains("3 descriptors"));
    assert!(refused(
        MovedKind::Codex,
        json!({ "provider": 7, "link": null }),
        vec![pipe(), pipe(), pipe()]
    )
    .contains("not readable"));
    assert!(refused(MovedKind::Acp, json!("a string"), vec![pipe(), pipe(), pipe()]).contains("not readable"));
    assert!(refused(
        MovedKind::Claude,
        json!({ "socket_path": 3 }),
        vec![pipe(), pipe(), pipe()]
    )
    .contains("not readable"));
    assert_eq!(
        MovedKind::from_wire("gemini"),
        None,
        "a kind this build does not move names nothing"
    );
}
