//! The real `codex app-server` against the scripted model, one test per behaviour Eitri relies on.
//!
//! Every test is `#[ignore]`d: it needs a `codex` on `PATH` (or `EITRI_CODEX`), and runs it in a private
//! `CODEX_HOME` whose model provider is the scripted stand-in on `127.0.0.1`. No account is used, no
//! request leaves the machine and no model turn is spent. Run them with
//!
//! ```text
//! cargo test -p agent --test codex_real_scripted -- --ignored --test-threads=1
//! ```
//!
//! under an environment without `TMUX`. The child is also started without it, without `NVIM` and
//! without any API key.
//!
//! - The scripted model answers under the catalog slug `gpt-6-luna`, which Codex knows, so the tool set
//!   it offers is the one a real model gets (code mode: `exec_command` nested in `functions.exec`).
//! - Scratch directories are under the test target's own directory and never under `/tmp`: Codex's
//!   sandbox always lets a command write there, which would blur every "outside the sandbox" file.
//! - What each test starts is what Eitri sends: `session_config`, `ThreadStartParams`, `TurnStartParams`.
//!   A raw object is used only where the test is about a form Eitri must not send.
//! - Each test prints the `codex --version` it ran against.

#[path = "support/codex_peer.rs"]
mod codex_peer;

use agent::external::DirTrust;
use agent::jsonrpc_link::Skipped;
use agent::providers::codex::family::{Family, FamilyEdge};
use agent::providers::codex::scripted_model::{ScriptedModel, Step};
use agent::providers::codex::wire::{method, ElicitationResponse};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use codex_peer::*;

// ------------------------------------------------------------------------------------ a dotted directory key

/// A directory key written dotted (`projects.<dir>.trust_level`) is split on every `.`, names no entry,
/// and Codex then reads the project as trusted and writes that into the user's own file. The nested form
/// is what Eitri sends.
#[test]
#[ignore = "runs the real codex (on PATH) against the scripted model; no login, no network"]
fn dotted_key_would_fail_open_nested_does_not() {
    let codex = real_codex();
    for dotted in [true, false] {
        let s = Scratch::new(if dotted { "b3-dotted" } else { "b3-nested" });
        let model = ScriptedModel::start(vec![]).expect("model");
        let home = s.dir("home");
        write_home(&home, &model, "");
        // A name with a dot and a space, the shape that breaks a dotted key.
        let project = s.dir("proj.dot dir");
        let ran = s.path("tool-server-ran");
        write_project(&project, &tool_server("eitri_marker", &marker_script(&s), &ran));

        let root = text_of(&project).to_string();
        let projects = if dotted {
            json!({ format!("projects.{root}.trust_level"): "untrusted" })
        } else {
            json!({ "projects": { root.clone(): { "trust_level": "untrusted" } } })
        };
        let mut peer = Peer::start(&codex, &home, &project, decline_all());
        let params = json!({
            "cwd": root,
            "approvalPolicy": "on-request",
            "approvalsReviewer": "user",
            "sandbox": "workspace-write",
            "config": projects,
        });
        let thread = start_thread(&mut peer, params);
        let effort = json_str(&thread.answer, "/reasoningEffort")
            .unwrap_or_default()
            .to_string();
        run_turn(&mut peer, &thread.id, "probe");
        peer.pump_for(Duration::from_secs(3));
        let ran_now = appears(&ran, Duration::from_secs(5));
        let written = std::fs::read_to_string(home.join("config.toml")).unwrap_or_default();
        peer.close();

        if dotted {
            assert!(
                ran_now,
                "the dotted key was expected to fail open: the project's tool server should run"
            );
            assert_eq!(effort, "minimal", "the project's own configuration should have applied");
            assert!(
                written.contains("trust_level = \"trusted\""),
                "Codex should have written a trusted entry into the user's file:\n{written}"
            );
        } else {
            assert!(
                !ran_now,
                "the nested form must keep the project's tool server from running"
            );
            assert_ne!(
                effort, "minimal",
                "the untrusted project's configuration must not apply"
            );
            assert!(
                !written.contains("[projects"),
                "nothing may be written into the user's file:\n{written}"
            );
        }
    }
}

// ------------------------------------------------------------------------------------ an untrusted start loads nothing

/// An untrusted start and the resume of its thread in a new process load no instructions, no skill (the
/// project's or the user's) and no tool server; a resume that carries no config loads them, which is
/// what shows the levers are what keeps them out.
#[test]
#[ignore = "runs the real codex (on PATH) against the scripted model; no login, no network"]
fn untrusted_start_and_resume_load_nothing() {
    let codex = real_codex();
    let s = Scratch::new("i1");
    let model = ScriptedModel::start(vec![]).expect("model");
    let home = s.dir("home");
    write_home(&home, &model, "");
    let project = s.dir("proj");
    let ran = s.path("tool-server-ran");
    write_project(&project, &tool_server("eitri_marker", &marker_script(&s), &ran));
    write_instructions_and_skills(&project, &home);

    let markers = [AGENTS_MARKER, SKILL_A, SKILL_B, SKILL_USER];
    let assert_none_seen = |model: &ScriptedModel, from: usize, what: &str| {
        let text = requests_text(model, from);
        for marker in markers {
            assert!(!text.contains(marker), "{what}: the model was sent {marker}");
        }
    };

    // The start.
    let mut peer = Peer::start(&codex, &home, &project, decline_all());
    let thread = start_thread(&mut peer, start_params(&project, DirTrust::Untrusted));
    assert_narrow_echo(&thread.answer);
    assert!(
        instruction_sources(&thread.answer).is_empty(),
        "the start loaded instruction files"
    );
    assert_eq!(run_turn(&mut peer, &thread.id, "probe"), "completed");
    assert_none_seen(&model, 0, "start");
    let after_start = model.requests().len();
    // EOF on an idle server: it goes at once.
    assert!(
        peer.close() < Duration::from_secs(3),
        "the server did not exit on stdin EOF"
    );

    // The resume in a new process.
    let mut peer = Peer::start(&codex, &home, &project, decline_all());
    let resumed = peer
        .call(
            method::THREAD_RESUME,
            resume_params(&thread.id, &project, DirTrust::Untrusted),
        )
        .expect("thread/resume");
    assert_narrow_echo(&resumed);
    assert!(
        instruction_sources(&resumed).is_empty(),
        "the resume loaded instruction files"
    );
    let turns_in_answer = resumed
        .pointer("/thread/turns")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    assert_eq!(turns_in_answer, 0, "the resume answer carried the history");
    assert_eq!(run_turn(&mut peer, &thread.id, "probe again"), "completed");
    assert_none_seen(&model, after_start, "resume");
    // The history is still there, to be paged.
    let listed = peer
        .call(method::THREAD_TURNS_LIST, json!({"threadId": thread.id}))
        .expect("thread/turns/list");
    assert_eq!(listed.get("data").and_then(Value::as_array).map(Vec::len), Some(2));
    let after_resume = model.requests().len();
    peer.close();

    // The control: a resume with no config at all loads what the lever keeps out.
    let mut peer = Peer::start(&codex, &home, &project, decline_all());
    let mut bare = resume_params(&thread.id, &project, DirTrust::Untrusted);
    bare.as_object_mut().expect("an object").remove("config");
    let control = peer.call(method::THREAD_RESUME, bare).expect("control resume");
    assert_eq!(run_turn(&mut peer, &thread.id, "control"), "completed");
    let text = requests_text(&model, after_resume);
    for marker in markers {
        assert!(
            text.contains(marker),
            "the control did not reach the model with {marker}: the fixture proves nothing"
        );
    }
    assert!(
        !instruction_sources(&control).is_empty(),
        "the control loaded no instruction file"
    );
    peer.close();

    assert!(!ran.exists(), "the project's tool server ran");
    assert!(
        !home_has_project_entry(&home),
        "a project entry was written into the user's file"
    );
}

// ------------------------------------------------------------------------------------ one command as the model's tool call

/// Runs `command` as the model's one tool call in a started thread and returns the output it reported.
fn run_one_command(
    codex: &Path,
    s: &Scratch,
    home_tables: &str,
    project_extra: &str,
    params: impl FnOnce(&Path) -> Value,
    eitri_turn: bool,
    command: String,
) -> (Value, String, ScriptedModel) {
    let model = ScriptedModel::start(vec![Step::exec("c1", &command, false).unless_body_has("\"c1\"")]).expect("model");
    let home = s.dir("home");
    write_home(&home, &model, home_tables);
    let project = s.dir("proj");
    write_project(&project, project_extra);
    let mut peer = Peer::start(codex, &home, &project, decline_all());
    let thread = start_thread(&mut peer, params(&project));
    if eitri_turn {
        assert_eq!(run_turn(&mut peer, &thread.id, "probe"), "completed");
    } else {
        // A turn with no per-turn sandbox policy: only what the thread was started with applies.
        let answer = peer
            .call(
                method::TURN_START,
                json!({"threadId": thread.id, "input": [{"type": "text", "text": "probe"}]}),
            )
            .expect("turn/start");
        let turn = json_str(&answer, "/turn/id").expect("a turn id").to_string();
        assert_eq!(
            peer.wait_turn(&thread.id, &turn, TURN_BUDGET).as_deref(),
            Some("completed")
        );
    }
    peer.close();
    let output = call_outputs(&model).remove("c1").unwrap_or_default();
    (thread.answer, output, model)
}

/// A command that tries a TCP connection to `port` and a write into `extra`, and says what happened.
fn reach_out_command(port: u16, extra: &Path) -> String {
    format!(
        "python3 - <<'PY'\nimport socket\ntry:\n    s = socket.socket()\n    s.settimeout(2)\n    s.connect(('127.0.0.1', {port}))\n    print('TCP_OK')\nexcept Exception as e:\n    print('TCP_FAIL', type(e).__name__)\nPY\n(touch {extra}/written && echo EXTRA_WRITE_OK) || echo EXTRA_WRITE_FAIL",
        extra = text_of(extra)
    )
}

/// A user's `[sandbox_workspace_write]` (network, an extra writable root) and a trusted project's widen
/// the sandbox unless the session states the narrow table, which Eitri always does.
#[test]
#[ignore = "runs the real codex (on PATH) against the scripted model; no login, no network"]
fn user_widening_is_pinned() {
    let codex = real_codex();
    // Each case: (name, the home widens, the project widens, the project's trust).
    for (name, widen_home, widen_project, trust) in [
        ("user", true, false, DirTrust::Untrusted),
        ("project", false, true, DirTrust::Trusted),
    ] {
        let project_tables = if widen_project {
            "\n[sandbox_workspace_write]\nnetwork_access = true\n"
        } else {
            ""
        };
        // (label, what the session is started with, whether the turn carries Eitri's own policy)
        for (label, pinned, eitri_turn) in [
            ("control", false, false),
            ("thread config only", true, false),
            ("what Eitri sends", true, true),
        ] {
            let s = Scratch::new(&format!("i2-{name}-{}", label.replace(' ', "-")));
            let extra = s.dir("extra");
            let home_tables = if widen_home {
                format!(
                    "\n[sandbox_workspace_write]\nnetwork_access = true\nwritable_roots = [\"{}\"]\n",
                    text_of(&extra)
                )
            } else {
                String::new()
            };
            let port_probe = ScriptedModel::start(vec![]).expect("a port to probe");
            let command = reach_out_command(port_probe.port, &extra);
            let (answer, output, _model) = run_one_command(
                &codex,
                &s,
                &home_tables,
                project_tables,
                |project| {
                    if pinned {
                        return start_params(project, trust);
                    }
                    // Only the projects entry: nothing states the sandbox's table.
                    let root = text_of(project).to_string();
                    let level = if trust == DirTrust::Trusted {
                        "trusted"
                    } else {
                        "untrusted"
                    };
                    json!({
                        "cwd": root,
                        "approvalPolicy": "on-request",
                        "approvalsReviewer": "user",
                        "sandbox": "workspace-write",
                        "config": { "projects": { root.clone(): { "trust_level": level } } },
                    })
                },
                eitri_turn,
                command,
            );
            let case = format!("{name}, {label}");
            if pinned {
                assert_narrow_echo(&answer);
                assert_eq!(
                    answer.pointer("/sandbox/networkAccess"),
                    Some(&json!(false)),
                    "{case}: {answer}"
                );
                assert_eq!(
                    answer.pointer("/sandbox/writableRoots"),
                    Some(&json!([])),
                    "{case}: {answer}"
                );
                assert!(
                    output.contains("TCP_FAIL"),
                    "{case}: a sandboxed command reached the network: {output}"
                );
                assert!(!output.contains("TCP_OK"), "{case}: {output}");
                assert!(
                    output.contains("EXTRA_WRITE_FAIL"),
                    "{case}: the extra root was writable: {output}"
                );
                assert!(
                    !extra.join("written").exists(),
                    "{case}: a file appeared in the extra root"
                );
            } else {
                // The control: it must widen, or the case shows nothing.
                assert_eq!(
                    answer.pointer("/sandbox/networkAccess"),
                    Some(&json!(true)),
                    "{case}: {answer}"
                );
                assert!(
                    output.contains("TCP_OK"),
                    "{case}: the control did not reach the port: {output}"
                );
                if widen_home {
                    assert!(
                        output.contains("EXTRA_WRITE_OK"),
                        "{case}: the control could not write: {output}"
                    );
                }
            }
        }
    }
}

/// A user who turns the extra-permission features on does not get them in a session: the tool schema
/// the model is offered has no `with_additional_permissions` and there is no `request_permissions` tool.
#[test]
#[ignore = "runs the real codex (on PATH) against the scripted model; no login, no network"]
fn features_forced_off() {
    let codex = real_codex();
    let features = "\n[features]\nexec_permission_approvals = true\nrequest_permissions_tool = true\n";

    // The control: no `features` override. The extra-permission field is then part of the schema.
    let s = Scratch::new("b2-control");
    let model = ScriptedModel::start(vec![]).expect("model");
    let home = s.dir("home");
    write_home(&home, &model, features);
    let project = s.dir("proj");
    write_project(&project, "");
    let root = text_of(&project).to_string();
    let mut peer = Peer::start(&codex, &home, &project, decline_all());
    let thread = start_thread(
        &mut peer,
        json!({
            "cwd": root, "approvalPolicy": "on-request", "approvalsReviewer": "user", "sandbox": "workspace-write",
            "config": { "projects": { root.clone(): { "trust_level": "untrusted" } } },
        }),
    );
    run_turn(&mut peer, &thread.id, "probe");
    peer.close();
    let text = requests_text(&model, 0);
    assert!(
        text.contains("with_additional_permissions"),
        "the control schema did not carry the field: the user's feature switch no longer matters, re-measure"
    );

    // What Eitri sends.
    let s = Scratch::new("b2-forced");
    let model = ScriptedModel::start(vec![]).expect("model");
    let home = s.dir("home");
    write_home(&home, &model, features);
    let project = s.dir("proj");
    write_project(&project, "");
    let mut peer = Peer::start(&codex, &home, &project, decline_all());
    let thread = start_thread(&mut peer, start_params(&project, DirTrust::Untrusted));
    assert_eq!(run_turn(&mut peer, &thread.id, "probe"), "completed");
    peer.close();
    let text = requests_text(&model, 0);
    assert!(
        !text.contains("with_additional_permissions"),
        "the extra-permission field reached the model"
    );
    assert!(
        !text.contains("\"request_permissions\""),
        "a request_permissions tool reached the model"
    );
}

// ------------------------------------------------------------------------------------ hooks

/// The key and hash Codex gives a project's hook, learnt from a throwaway home that trusts the project.
fn learn_hook(codex: &Path, s: &Scratch, model: &ScriptedModel, project: &Path, tag: &str) -> (String, String) {
    let probe_home = s.dir(&format!("probe-home-{tag}"));
    write_home(
        &probe_home,
        model,
        &format!("\n[projects.\"{}\"]\ntrust_level = \"trusted\"\n", text_of(project)),
    );
    let mut peer = Peer::start(codex, &probe_home, project, decline_all());
    let listed = peer
        .call("hooks/list", json!({ "cwds": [text_of(project)] }))
        .expect("hooks/list");
    peer.close();
    find_hook(&listed, text_of(project)).expect("Codex listed the project's hook")
}

fn find_hook(value: &Value, project: &str) -> Option<(String, String)> {
    match value {
        Value::Object(map) => {
            if let (Some(Value::String(key)), Some(Value::String(hash))) = (map.get("key"), map.get("currentHash")) {
                if key.starts_with(project) {
                    return Some((key.clone(), hash.clone()));
                }
            }
            map.values().find_map(|v| find_hook(v, project))
        }
        Value::Array(items) => items.iter().find_map(|v| find_hook(v, project)),
        _ => None,
    }
}

/// What a project's hook and exec-policy rule do, by trust and by whether the user trusted the hook.
struct HookRun {
    hook_ran: bool,
    hook_notes: usize,
    rule_file_created: bool,
    approval_requests: usize,
    status: String,
}

fn hook_run(codex: &Path, tag: &str, trust: DirTrust, hook_trusted_by_user: bool) -> HookRun {
    let s = Scratch::new(&format!("i14-{tag}"));
    let outside = s.dir("outside");
    let target = outside.join(format!("rule-{tag}"));
    let model = ScriptedModel::start(vec![
        Step::exec("c1", &format!("touch {}", text_of(&target)), false).unless_body_has("\"c1\"")
    ])
    .expect("model");
    let project = s.dir("proj");
    write_project(&project, "");
    let ran = s.path("hook-ran");
    let marker = marker_script(&s);
    write_file(
        &project.join(".codex/hooks.json"),
        &json!({"hooks": {"PreToolUse": [{"matcher": "*", "hooks": [{
            "type": "command",
            "command": format!("/bin/sh '{}' '{}'", text_of(&marker), text_of(&ran)),
            "timeout": 5,
        }]}]}})
        .to_string(),
    );
    write_file(
        &project.join(".codex/rules/default.rules"),
        &format!(
            "prefix_rule(pattern=[\"touch\", \"{}\"], decision=\"allow\")\n",
            text_of(&target)
        ),
    );
    let home = s.dir("home");
    let mut tables = String::new();
    if hook_trusted_by_user {
        let (key, hash) = learn_hook(codex, &s, &model, &project, tag);
        tables = format!("\n[hooks.state.\"{key}\"]\ntrusted_hash = \"{hash}\"\nenabled = true\n");
    }
    write_home(&home, &model, &tables);

    let mut peer = Peer::start(codex, &home, &project, decline_all());
    let thread = start_thread(&mut peer, start_params(&project, trust));
    let status = run_turn(&mut peer, &thread.id, "probe");
    peer.pump_for(Duration::from_secs(1));
    let result = HookRun {
        hook_ran: ran.exists(),
        hook_notes: peer.notes.iter().filter(|n| n.method.starts_with("hook/")).count(),
        rule_file_created: target.exists(),
        approval_requests: peer.requests.iter().filter(|r| is_decision_request(&r.method)).count(),
        status,
    };
    peer.close();
    result
}

/// A trusted project's hook (trusted by hash in the user's file) runs and its rule lets a command out of
/// the sandbox with no request; an untrusted project's neither; a trusted project whose hook the user did
/// not trust runs the rule but not the hook.
#[test]
#[ignore = "runs the real codex (on PATH) against the scripted model; no login, no network"]
fn hooks_and_rules_trusted_and_untrusted() {
    let codex = real_codex();

    let trusted = hook_run(&codex, "t", DirTrust::Trusted, true);
    assert_eq!(trusted.status, "completed");
    assert!(trusted.hook_ran, "the trusted project's hook did not run");
    assert!(trusted.hook_notes > 0, "no hook notification arrived");
    assert!(
        trusted.rule_file_created,
        "the trusted project's allow rule did not apply"
    );
    assert_eq!(
        trusted.approval_requests, 0,
        "the rule should have allowed the command with no request"
    );

    let untrusted = hook_run(&codex, "u", DirTrust::Untrusted, true);
    assert_eq!(untrusted.status, "completed");
    assert!(!untrusted.hook_ran, "an untrusted project's hook ran");
    assert_eq!(untrusted.hook_notes, 0, "an untrusted project's hook was announced");
    assert!(!untrusted.rule_file_created, "an untrusted project's rule applied");
    assert_eq!(untrusted.approval_requests, 0);

    let unhashed = hook_run(&codex, "n", DirTrust::Trusted, false);
    assert_eq!(unhashed.status, "completed");
    assert!(!unhashed.hook_ran, "a hook the user did not trust ran");
    assert!(
        unhashed.rule_file_created,
        "the trusted project's rule should apply without the hook"
    );
}

// ------------------------------------------------------------------------------------ approvals

/// A command that asks to leave the sandbox reaches the client as a request; accept runs it, decline does
/// not and the turn goes on, cancel does not and ends the turn.
#[test]
#[ignore = "runs the real codex (on PATH) against the scripted model; no login, no network"]
fn approvals_accept_decline_cancel() {
    let codex = real_codex();
    let s = Scratch::new("approvals");
    let outside = s.dir("outside");
    let file = |name: &str| outside.join(name);
    let escalate = |call: &str, prompt: &str, name: &str| {
        Step::exec(call, &format!("touch {}", text_of(&file(name))), true)
            .when_body_has(prompt)
            .unless_body_has(&format!("\"{call}\""))
    };
    let model = ScriptedModel::start(vec![
        escalate("a1", "PROMPT-ACCEPT", "accepted"),
        escalate("d1", "PROMPT-DECLINE", "declined"),
        escalate("x1", "PROMPT-CANCEL", "cancelled"),
    ])
    .expect("model");
    let home = s.dir("home");
    write_home(&home, &model, "");
    let project = s.dir("proj");
    write_project(&project, "");

    // The command's text says which answer it gets.
    let answer: Answer = Box::new(|req| {
        let command = req.params.get("command").and_then(Value::as_str).unwrap_or_default();
        let decision = if command.contains("accepted") {
            "accept"
        } else if command.contains("cancelled") {
            "cancel"
        } else {
            "decline"
        };
        Some(json!({ "decision": decision }))
    });
    let mut peer = Peer::start(&codex, &home, &project, answer);
    let thread = start_thread(&mut peer, start_params(&project, DirTrust::Untrusted));

    assert_eq!(run_turn(&mut peer, &thread.id, "PROMPT-ACCEPT"), "completed");
    assert!(file("accepted").exists(), "an accepted command did not run");

    assert_eq!(run_turn(&mut peer, &thread.id, "PROMPT-DECLINE"), "completed");
    assert!(!file("declined").exists(), "a declined command ran");
    // The model was told, so the turn went on to its next request.
    let declined_output = call_outputs(&model).remove("d1").unwrap_or_default();
    assert!(
        !declined_output.is_empty(),
        "the model was not told the command was declined"
    );

    assert_eq!(
        run_turn(&mut peer, &thread.id, "PROMPT-CANCEL"),
        "interrupted",
        "a cancelled approval should end the turn by itself"
    );
    assert!(!file("cancelled").exists(), "a cancelled command ran");

    let asked: Vec<&Req> = peer
        .requests
        .iter()
        .filter(|r| r.method == method::COMMAND_APPROVAL)
        .collect();
    assert_eq!(
        asked.len(),
        3,
        "each escalation should have been one command approval request"
    );
    for req in &asked {
        assert_eq!(json_str(&req.params, "/threadId"), Some(thread.id.as_str()));
    }
    peer.close();
}

// ------------------------------------------------------------------------------------ interrupts

/// Interrupting: a running turn ends `interrupted` at once; an approval left unanswered is resolved by the
/// interrupt and a late accept runs nothing; an interrupt for a finished turn or an unknown id is an
/// error at once; a second interrupt for an already interrupted turn is not answered until the next turn
/// ends.
#[test]
#[ignore = "runs the real codex (on PATH) against the scripted model; no login, no network"]
fn interrupt_waiting_approval_and_double_interrupt() {
    let codex = real_codex();
    let s = Scratch::new("i10");
    let outside = s.dir("outside");
    let late = outside.join("late-accept");
    let hang_for = Duration::from_secs(60);
    let model = ScriptedModel::start(vec![
        // Turn 1: the model stalls. Turn 2: an approval nobody answers. Turn 3: stalls. Turn 4 (and
        // later turns): the default one-line answer.
        Step::events(vec![json!({"type": "response.created", "response": {"id": "{RID}"}})])
            .when_body_has("PROMPT-ONE")
            .unless_body_has("PROMPT-TWO")
            .then_hang(hang_for),
        Step::exec("e1", &format!("touch {}", text_of(&late)), true)
            .when_body_has("PROMPT-TWO")
            .unless_body_has("\"e1\"")
            .unless_body_has("PROMPT-THREE"),
        Step::events(vec![json!({"type": "response.created", "response": {"id": "{RID}"}})])
            .when_body_has("PROMPT-THREE")
            .unless_body_has("PROMPT-FOUR")
            .then_hang(hang_for),
    ])
    .expect("model");
    let home = s.dir("home");
    write_home(&home, &model, "");
    let project = s.dir("proj");
    write_project(&project, "");

    // No answer to an approval unless the test gives one.
    let mut peer = Peer::start(&codex, &home, &project, Box::new(|_| None));
    let thread = start_thread(&mut peer, start_params(&project, DirTrust::Untrusted));

    // A running turn.
    let turn1 = send_turn(&mut peer, &thread.id, "PROMPT-ONE");
    assert!(peer.wait_for(Duration::from_secs(20), |p| p.turn_started(&thread.id)));
    peer.pump_for(Duration::from_secs(1));
    let at = Instant::now();
    let answer = peer
        .call(method::TURN_INTERRUPT, json!({"threadId": thread.id, "turnId": turn1}))
        .expect("turn/interrupt");
    assert_eq!(answer, json!({}));
    assert_eq!(
        peer.wait_turn(&thread.id, &turn1, Duration::from_secs(20)).as_deref(),
        Some("interrupted")
    );
    assert!(
        at.elapsed() < Duration::from_secs(10),
        "the interrupt took {:?}",
        at.elapsed()
    );

    peer.pump_for(Duration::from_secs(1));
    // An interrupt that names another turn than the active one is an error, at once.
    let error = peer
        .call(
            method::TURN_INTERRUPT,
            json!({"threadId": thread.id, "turnId": "not-a-turn"}),
        )
        .expect_err("an unknown turn");
    // The interrupted turn is still the thread's active one until the stalled request is let go, so an
    // unknown id is told what the active turn is.
    assert!(error.contains("active turn"), "{error}");

    // An approval nobody answers, then an interrupt.
    let turn2 = send_turn(&mut peer, &thread.id, "PROMPT-TWO");
    assert!(peer.wait_for(Duration::from_secs(30), |p| {
        p.requests.iter().any(|r| r.method == method::COMMAND_APPROVAL)
    }));
    let request = peer
        .requests
        .iter()
        .find(|r| r.method == method::COMMAND_APPROVAL)
        .cloned()
        .expect("the request");
    let answer = peer
        .call(method::TURN_INTERRUPT, json!({"threadId": thread.id, "turnId": turn2}))
        .expect("turn/interrupt with an approval waiting");
    assert_eq!(answer, json!({}));
    assert_eq!(
        peer.wait_turn(&thread.id, &turn2, Duration::from_secs(20)).as_deref(),
        Some("interrupted")
    );
    peer.pump_for(Duration::from_secs(1));
    assert!(
        peer.note(method::SERVER_REQUEST_RESOLVED).is_some(),
        "the interrupt did not resolve the waiting request"
    );
    // A late accept runs nothing.
    peer.link.respond(request.id, json!({"decision": "accept"}));
    peer.pump_for(Duration::from_secs(2));
    assert!(!late.exists(), "a late accept ran the command");

    // A second interrupt of an interrupted turn is not answered until the next turn ends.
    let turn3 = send_turn(&mut peer, &thread.id, "PROMPT-THREE");
    assert!(peer.wait_for(Duration::from_secs(20), |p| p.turn_started(&thread.id)
        && p.notes.len() > 4));
    peer.pump_for(Duration::from_secs(1));
    let first = peer
        .call(method::TURN_INTERRUPT, json!({"threadId": thread.id, "turnId": turn3}))
        .expect("the first interrupt");
    assert_eq!(first, json!({}));
    assert_eq!(
        peer.wait_turn(&thread.id, &turn3, Duration::from_secs(20)).as_deref(),
        Some("interrupted")
    );
    let mut stale = peer
        .link
        .request(method::TURN_INTERRUPT, json!({"threadId": thread.id, "turnId": turn3}));
    let mut answered = None;
    let end = Instant::now() + Duration::from_secs(3);
    while Instant::now() < end {
        peer.pump();
        if let Some(a) = stale.try_take() {
            answered = Some(a);
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        answered.is_none(),
        "a second interrupt was answered at once: {answered:?}"
    );
    let turn4 = send_turn(&mut peer, &thread.id, "PROMPT-FOUR");
    assert_eq!(
        peer.wait_turn(&thread.id, &turn4, TURN_BUDGET).as_deref(),
        Some("completed")
    );
    peer.pump_for(Duration::from_secs(1));
    let end = Instant::now() + Duration::from_secs(5);
    while answered.is_none() && Instant::now() < end {
        peer.pump();
        answered = stale.try_take();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        answered
            .expect("the stale interrupt is answered once the next turn ended")
            .ok(),
        Some(json!({}))
    );
    // An interrupt for a turn that completed on its own is an error, at once.
    let error = peer
        .call(method::TURN_INTERRUPT, json!({"threadId": thread.id, "turnId": turn4}))
        .expect_err("a finished turn");
    assert!(error.contains("no active turn to interrupt"), "{error}");
    peer.close();
}

// ------------------------------------------------------------------------------------ closing the server

/// Closing stdin ends the server at once, idle or mid-turn, and takes a live project tool server with it;
/// a SIGKILL of the server alone leaves that tool server running, and the thread resumes in a new process
/// with the killed turn read as interrupted.
#[test]
#[ignore = "runs the real codex (on PATH) against the scripted model; no login, no network"]
fn eof_and_sigkill_shutdown() {
    let codex = real_codex();

    // EOF in the middle of a turn, with a live project tool server.
    let s = Scratch::new("i13-eof");
    let pid_file = s.path("sleeper.pid");
    let _guard = KillOnDrop(pid_file.clone());
    let model = ScriptedModel::start(vec![Step::events(vec![
        json!({"type": "response.created", "response": {"id": "{RID}"}}),
    ])
    .then_hang(Duration::from_secs(60))])
    .expect("model");
    let home = s.dir("home");
    write_home(&home, &model, "");
    let project = s.dir("proj");
    write_project(&project, &local_tool_server(&s, "sleeper", &pid_file, false));
    let mut peer = Peer::start(&codex, &home, &project, decline_all());
    let thread = start_thread(&mut peer, start_params(&project, DirTrust::Trusted));
    send_turn(&mut peer, &thread.id, "probe");
    assert!(peer.wait_for(Duration::from_secs(20), |p| p.turn_started(&thread.id)));
    assert!(
        appears(&pid_file, Duration::from_secs(10)),
        "the project's tool server never started"
    );
    assert!(
        model_has_requests(&model, 1, Duration::from_secs(15)),
        "the model never received the turn's request"
    );
    let sleeper = pid_in(&pid_file).expect("the tool server's pid");
    let took = peer.close();
    assert!(
        took < Duration::from_secs(3),
        "the server took {took:?} to exit on stdin EOF mid-turn"
    );
    let end = Instant::now() + Duration::from_secs(3);
    while process_alive(sleeper) && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !process_alive(sleeper),
        "the project's tool server outlived a clean shutdown"
    );

    // SIGKILL of the server mid-turn, then a resume in a new process.
    let s = Scratch::new("i13-kill");
    let pid_file = s.path("sleeper.pid");
    let _guard = KillOnDrop(pid_file.clone());
    let model = ScriptedModel::start(vec![Step::events(vec![
        json!({"type": "response.created", "response": {"id": "{RID}"}}),
    ])
    .then_hang(Duration::from_secs(60))])
    .expect("model");
    let home = s.dir("home");
    write_home(&home, &model, "");
    let project = s.dir("proj");
    write_project(&project, &local_tool_server(&s, "sleeper", &pid_file, true));
    let mut peer = Peer::start(&codex, &home, &project, decline_all());
    let thread = start_thread(&mut peer, start_params(&project, DirTrust::Trusted));
    send_turn(&mut peer, &thread.id, "first");
    assert!(peer.wait_for(Duration::from_secs(20), |p| p.turn_started(&thread.id)));
    assert!(
        appears(&pid_file, Duration::from_secs(10)),
        "the project's tool server never started"
    );
    assert!(
        model_has_requests(&model, 1, Duration::from_secs(15)),
        "the model never received the turn's request"
    );
    peer.pump_for(Duration::from_secs(1));
    let locks: Vec<String> = std::fs::read_dir(home.join("thread-writer-locks"))
        .map(|d| {
            d.flatten()
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        locks.iter().any(|name| name.contains(&thread.id)),
        "no writer lock for the thread during its turn: {locks:?}"
    );
    let server = peer.link.pid() as i32;
    // SAFETY: the pid is the child this test spawned and still holds.
    unsafe { libc::kill(server, libc::SIGKILL) };
    assert!(peer.wait_for(Duration::from_secs(5), |p| p.closed.is_some()));
    peer.close();
    let sleeper = pid_in(&pid_file).expect("the tool server's pid");
    // The orphan is ended by the pid captured now, not by the file, which a later start overwrites.
    let _orphan = KillPid(sleeper);
    assert!(
        process_alive(sleeper),
        "a SIGKILL of the server alone was expected to orphan its tool server"
    );

    let mut peer = Peer::start(&codex, &home, &project, decline_all());
    let resumed = peer
        .call(
            method::THREAD_RESUME,
            resume_params(&thread.id, &project, DirTrust::Trusted),
        )
        .expect("the thread resumes after a SIGKILL");
    assert_narrow_echo(&resumed);
    let listed = peer
        .call(method::THREAD_TURNS_LIST, json!({"threadId": thread.id}))
        .expect("thread/turns/list");
    let statuses: Vec<&str> = listed
        .get("data")
        .and_then(Value::as_array)
        .map(|turns| {
            turns
                .iter()
                .filter_map(|t| t.get("status").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(
        statuses,
        vec!["interrupted"],
        "the killed turn should read as interrupted"
    );
    assert_eq!(run_turn(&mut peer, &thread.id, "second"), "completed");
    peer.close();
}

// ------------------------------------------------------------------------------------ subagents

const PARENT_PROMPT: &str = "REAL-PARENT-PROMPT";
const CHILD_TASK: &str = "REAL-CHILD-TASK";
const GRAND_TASK: &str = "REAL-GRAND-TASK";

fn spawn_step(call: &str, name: &str, task: &str) -> Step {
    Step::call(
        call,
        Some(COLLAB),
        "spawn_agent",
        &json!({"task_name": name, "message": task, "fork_turns": "none"}),
    )
}

/// What a family run left behind.
struct FamilyRun {
    parent: String,
    peer: Peer,
    model: ScriptedModel,
    outside: PathBuf,
    project: PathBuf,
    ran: PathBuf,
}

/// The parent spawns a child and waits for it; the child touches a file by a rule, by a plain write, then
/// asks to leave the sandbox, which the client declines.
fn family_run(codex: &Path, s: &Scratch, trust: DirTrust) -> FamilyRun {
    let outside = s.dir("outside");
    let out = text_of(&outside).to_string();
    let for_child = |step: Step| step.when_body_has(CHILD_TASK).unless_body_has(PARENT_PROMPT);
    let model = ScriptedModel::start(vec![
        spawn_step("sp1", "child", &format!("{CHILD_TASK} run the commands"))
            .when_body_has(PARENT_PROMPT)
            .unless_body_has("\"sp1\""),
        Step::call("wa1", Some(COLLAB), "wait_agent", &json!({"timeout_ms": 30000}))
            .when_body_has("\"sp1\"")
            .unless_body_has("\"wa1\""),
        for_child(Step::exec("cc1", &format!("touch {out}/child-rule"), false)).unless_body_has("\"cc1\""),
        for_child(Step::exec("cc0", &format!("touch {out}/child-plain"), false))
            .when_body_has("\"cc1\"")
            .unless_body_has("\"cc0\""),
        for_child(Step::exec("cc2", &format!("touch {out}/child-escalated"), true))
            .when_body_has("\"cc0\"")
            .unless_body_has("\"cc2\""),
    ])
    .expect("model");
    let home = s.dir("home");
    write_home(&home, &model, "");
    let project = s.dir("proj");
    let ran = s.path("tool-server-ran");
    write_project(&project, &tool_server("eitri_marker", &marker_script(s), &ran));
    write_instructions_and_skills(&project, &home);
    write_file(
        &project.join(".codex/rules/default.rules"),
        &format!("prefix_rule(pattern=[\"touch\", \"{out}/child-rule\"], decision=\"allow\")\n"),
    );
    let mut peer = Peer::start(codex, &home, &project, decline_all());
    let thread = start_thread(&mut peer, start_params(&project, trust));
    let turn = send_turn(&mut peer, &thread.id, PARENT_PROMPT);
    assert!(
        peer.wait_turn(&thread.id, &turn, TURN_BUDGET).is_some(),
        "the parent's turn did not end; stderr: {}",
        peer.stderr_tail()
    );
    // The child's turn may end after the parent's; let everything settle.
    peer.pump_for(Duration::from_secs(5));
    FamilyRun {
        parent: thread.id,
        peer,
        model,
        outside,
        project,
        ran,
    }
}

/// A child inherits its parent's trust and sandbox: it has its own thread id on everything it does, and
/// it neither sees the project's instructions and skills, nor starts its tool server, nor applies its
/// rule when the parent is untrusted. A trusted parent's child gets all of those.
#[test]
#[ignore = "runs the real codex (on PATH) against the scripted model; no login, no network"]
fn child_inherits_and_has_its_own_thread_id() {
    let codex = real_codex();
    for trust in [DirTrust::Untrusted, DirTrust::Trusted] {
        let trusted = trust == DirTrust::Trusted;
        let s = Scratch::new(if trusted { "i11-trusted" } else { "i11-untrusted" });
        let run = family_run(&codex, &s, trust);
        let what = if trusted { "trusted" } else { "untrusted" };

        // The child was announced from the parent's thread, and announced first.
        let children = run.peer.announced_children();
        assert!(!children.is_empty(), "{what}: no child was announced");
        let (announced_on, child) = children[0].clone();
        assert_eq!(
            announced_on, run.parent,
            "{what}: the child was announced on another thread"
        );
        assert_ne!(child, run.parent);

        // What reached the model for the child.
        let child_requests: Vec<Value> = run
            .model
            .requests()
            .into_iter()
            .filter(|b| {
                let text = b.to_string();
                text.contains(CHILD_TASK) && !text.contains(PARENT_PROMPT)
            })
            .collect();
        assert!(
            !child_requests.is_empty(),
            "{what}: the child sent no request of its own"
        );
        let child_text: String = child_requests.iter().map(Value::to_string).collect();
        let effort = child_requests[0]
            .pointer("/reasoning/effort")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let outputs = call_outputs(&run.model);

        // The child's own turn and its escalation carry its thread id, and the decline was honoured.
        let child_started = run
            .peer
            .notes
            .iter()
            .any(|n| n.method == method::TURN_STARTED && json_str(&n.params, "/threadId") == Some(child.as_str()));
        let child_ended =
            run.peer.notes.iter().any(|n| {
                n.method == method::TURN_COMPLETED && json_str(&n.params, "/threadId") == Some(child.as_str())
            });
        assert!(
            child_started && child_ended,
            "{what}: the child's turn events lacked its thread id"
        );
        let escalations: Vec<&Req> = run
            .peer
            .requests
            .iter()
            .filter(|r| r.method == method::COMMAND_APPROVAL)
            .collect();
        assert_eq!(
            escalations.len(),
            1,
            "{what}: expected one escalation, got {}",
            escalations.len()
        );
        assert_eq!(
            json_str(&escalations[0].params, "/threadId"),
            Some(child.as_str()),
            "{what}: the escalation did not carry the child's thread id"
        );
        assert!(
            run.peer.notes.iter().any(|n| {
                n.method == method::SERVER_REQUEST_RESOLVED && json_str(&n.params, "/threadId") == Some(child.as_str())
            }),
            "{what}: the child's request was not resolved on the child's thread"
        );
        assert!(
            !run.outside.join("child-escalated").exists(),
            "{what}: a declined escalation ran"
        );
        assert!(
            !run.outside.join("child-plain").exists(),
            "{what}: a plain write outside the root succeeded"
        );
        assert!(
            outputs.contains_key("cc1"),
            "{what}: the child's first command never reported back"
        );
        assert!(
            !child_text.contains("with_additional_permissions"),
            "{what}: the child was offered the extra-permission field"
        );

        let rule_applied = run.outside.join("child-rule").exists();
        let tool_server_ran = appears(&run.ran, Duration::from_secs(2));
        if trusted {
            assert_eq!(
                effort, "minimal",
                "trusted: the project's configuration should apply to the child"
            );
            assert!(
                child_text.contains(AGENTS_MARKER),
                "trusted: the child did not see the instructions"
            );
            assert!(child_text.contains(SKILL_B), "trusted: the child did not see the skill");
            assert!(tool_server_ran, "trusted: the project's tool server did not start");
            assert!(rule_applied, "trusted: the project's rule did not apply to the child");
        } else {
            assert_ne!(
                effort, "minimal",
                "untrusted: the project's configuration reached the child"
            );
            assert!(
                !child_text.contains(AGENTS_MARKER),
                "untrusted: the child saw the instructions"
            );
            assert!(!child_text.contains(SKILL_B), "untrusted: the child saw the skill");
            assert!(
                !child_text.contains(SKILL_USER),
                "untrusted: the child saw the user's skill"
            );
            assert!(!tool_server_ran, "untrusted: the project's tool server started");
            assert!(!rule_applied, "untrusted: the project's rule applied to the child");
        }
        let _ = &run.project;
        run.peer.close();
    }
}

/// One thread's event as the family bookkeeping reads it, from the notifications in the order they came.
fn feed_family(family: &mut Family, notes: &[Note]) -> Vec<(Instant, String, FamilyEdge)> {
    let mut edges = Vec::new();
    for note in notes {
        let thread = json_str(&note.params, "/threadId").unwrap_or_default().to_string();
        match note.method.as_str() {
            m if m == method::ITEM_STARTED || m == method::ITEM_COMPLETED => {
                if json_str(&note.params, "/item/type") == Some("subAgentActivity") {
                    let item = json!({
                        "id": json_str(&note.params, "/item/id").unwrap_or("x"),
                        "kind": json_str(&note.params, "/item/kind").unwrap_or_default(),
                        "agentThreadId": json_str(&note.params, "/item/agentThreadId").unwrap_or_default(),
                        "agentPath": json_str(&note.params, "/item/agentPath").unwrap_or_default(),
                    });
                    if let Ok(item) = serde_json::from_value(item) {
                        family.announce(&thread, &item);
                    }
                }
            }
            m if m == method::TURN_STARTED => {
                let turn = json_str(&note.params, "/turn/id").unwrap_or_default();
                edges.push((note.at, format!("start {thread}"), family.turn_started(&thread, turn)));
            }
            m if m == method::TURN_COMPLETED => {
                let turn = json_str(&note.params, "/turn/id").unwrap_or_default();
                edges.push((note.at, format!("end {thread}"), family.turn_completed(&thread, turn)));
            }
            _ => {}
        }
    }
    edges
}

/// A parent that spawns a child and ends its turn without waiting leaves the child running: it writes into
/// the project after the parent's turn ended, announces a grandchild on its own thread, and the grandchild's
/// request carries the grandchild's thread id. The family bookkeeping, fed what Codex said, keeps the tab's
/// turn open until the last of them ends.
#[test]
#[ignore = "runs the real codex (on PATH) against the scripted model; no login, no network"]
fn child_outlives_parent_turn_and_family_holds_the_turn() {
    let codex = real_codex();
    let s = Scratch::new("child-outlives-a");
    let outside = s.dir("outside");
    let for_child = |step: Step| step.when_body_has(CHILD_TASK).unless_body_has(PARENT_PROMPT);
    let model = ScriptedModel::start(vec![
        spawn_step("sp1", "child", CHILD_TASK)
            .when_body_has(PARENT_PROMPT)
            .unless_body_has("\"sp1\""),
        // The child: a command that finishes five seconds from now, then a grandchild.
        for_child(Step::exec("cs1", "sleep 5; touch child-late-write", false)).unless_body_has("\"cs1\""),
        for_child(spawn_step("gs1", "grand", GRAND_TASK))
            .when_body_has("\"cs1\"")
            .unless_body_has("\"gs1\""),
        Step::exec("gc1", &format!("touch {}/grand-escalated", text_of(&outside)), true)
            .when_body_has(GRAND_TASK)
            .unless_body_has(CHILD_TASK)
            .unless_body_has("\"gc1\""),
    ])
    .expect("model");
    let home = s.dir("home");
    write_home(&home, &model, "");
    let project = s.dir("proj");
    write_project(&project, "");
    let mut peer = Peer::start(&codex, &home, &project, decline_all());
    let thread = start_thread(&mut peer, start_params(&project, DirTrust::Untrusted));
    let turn = send_turn(&mut peer, &thread.id, PARENT_PROMPT);
    let status = peer
        .wait_turn(&thread.id, &turn, TURN_BUDGET)
        .expect("the parent's turn ended");
    let parent_ended = Instant::now();
    let late_file = project.join("child-late-write");
    let late_before = late_file.exists();
    peer.pump_for(Duration::from_secs(15));

    assert_eq!(status, "completed");
    assert!(
        !late_before,
        "the child had already written when the parent's turn ended"
    );
    assert!(
        late_file.exists(),
        "the child did not keep running after the parent's turn ended"
    );

    // The grandchild is announced on the child's thread, not the parent's.
    let children = peer.announced_children();
    let child = children
        .iter()
        .find(|(on, _)| *on == thread.id)
        .map(|(_, c)| c.clone())
        .expect("the child was announced on the parent's thread");
    let grand = children
        .iter()
        .find(|(on, _)| *on == child)
        .map(|(_, g)| g.clone())
        .expect("the grandchild was announced on the child's thread");
    let escalation = peer
        .requests
        .iter()
        .find(|r| r.method == method::COMMAND_APPROVAL)
        .expect("the grandchild's escalation");
    assert_eq!(json_str(&escalation.params, "/threadId"), Some(grand.as_str()));

    // The tab's turn: opened by the parent's turn, still open when the parent's turn ended, closed only
    // by the last turn of the family.
    let mut family = Family::new(thread.id.clone());
    let edges = feed_family(&mut family, &peer.notes);
    let parent_end = edges
        .iter()
        .find(|(_, what, _)| *what == format!("end {}", thread.id))
        .expect("the parent's end was seen");
    assert_eq!(
        parent_end.2,
        FamilyEdge::Still,
        "the tab's turn closed with the parent's turn"
    );
    let closed: Vec<&(Instant, String, FamilyEdge)> =
        edges.iter().filter(|(_, _, e)| *e == FamilyEdge::Closed).collect();
    assert_eq!(closed.len(), 1, "the tab's turn should close once: {edges:?}");
    assert!(
        closed[0].0 >= parent_ended + Duration::from_secs(2),
        "the tab's turn closed {:?} after the parent's, before the child could have finished",
        closed[0].0.duration_since(parent_ended)
    );
    assert!(family.running_turns().is_empty());
    peer.close();
}

/// Scripts a parent that waits on a child whose model request hangs.
fn waiting_family(hang: Duration) -> Vec<Step> {
    vec![
        spawn_step("sp1", "child", CHILD_TASK)
            .when_body_has(PARENT_PROMPT)
            .unless_body_has("\"sp1\""),
        Step::call("wa1", Some(COLLAB), "wait_agent", &json!({"timeout_ms": 60000}))
            .when_body_has("\"sp1\"")
            .unless_body_has("\"wa1\""),
        Step::events(vec![json!({"type": "response.created", "response": {"id": "{RID}"}})])
            .when_body_has(CHILD_TASK)
            .unless_body_has(PARENT_PROMPT)
            .then_hang(hang),
    ]
}

/// Interrupting the parent while it waits ends the parent's turn but not the child's, which is still
/// running; interrupting the child's own turn by its thread id stops it.
#[test]
#[ignore = "runs the real codex (on PATH) against the scripted model; no login, no network"]
fn interrupting_the_family_stops_the_child() {
    let codex = real_codex();
    for interrupt_child in [false, true] {
        let s = Scratch::new(if interrupt_child {
            "child-interrupted"
        } else {
            "child-outlives-b"
        });
        let model = ScriptedModel::start(waiting_family(Duration::from_secs(40))).expect("model");
        let home = s.dir("home");
        write_home(&home, &model, "");
        let project = s.dir("proj");
        write_project(&project, "");
        let mut peer = Peer::start(&codex, &home, &project, decline_all());
        let thread = start_thread(&mut peer, start_params(&project, DirTrust::Untrusted));
        let parent_turn = send_turn(&mut peer, &thread.id, PARENT_PROMPT);
        let parent = thread.id.clone();
        assert!(
            peer.wait_for(Duration::from_secs(30), |p| {
                p.notes.iter().any(|n| {
                    n.method == method::TURN_STARTED && json_str(&n.params, "/threadId") != Some(parent.as_str())
                })
            }),
            "the child's turn never started; stderr: {}",
            peer.stderr_tail()
        );
        peer.pump_for(Duration::from_secs(1));
        let child_start = peer
            .notes
            .iter()
            .find(|n| n.method == method::TURN_STARTED && json_str(&n.params, "/threadId") != Some(parent.as_str()))
            .cloned()
            .expect("the child's turn start");
        let child = json_str(&child_start.params, "/threadId")
            .expect("child thread")
            .to_string();
        let child_turn = json_str(&child_start.params, "/turn/id")
            .expect("child turn")
            .to_string();

        if interrupt_child {
            let answer = peer
                .call(method::TURN_INTERRUPT, json!({"threadId": child, "turnId": child_turn}))
                .expect("interrupting the child");
            assert_eq!(answer, json!({}));
            assert_eq!(
                peer.wait_turn(&child, &child_turn, Duration::from_secs(10)).as_deref(),
                Some("interrupted"),
                "the child's own interrupt did not stop it"
            );
        } else {
            let answer = peer
                .call(
                    method::TURN_INTERRUPT,
                    json!({"threadId": parent, "turnId": parent_turn}),
                )
                .expect("interrupting the parent");
            assert_eq!(answer, json!({}));
            assert_eq!(
                peer.wait_turn(&parent, &parent_turn, Duration::from_secs(10))
                    .as_deref(),
                Some("interrupted")
            );
            peer.pump_for(Duration::from_secs(8));
            assert!(
                peer.turn_status(&child, &child_turn).is_none(),
                "the parent's interrupt was expected to leave the child's turn running"
            );
            // The family bookkeeping still counts it, so Eitri must interrupt it by its own id.
            let mut family = Family::new(parent.clone());
            feed_family(&mut family, &peer.notes);
            assert_eq!(family.running_on(&child), Some(child_turn.as_str()));
        }
        peer.close();
    }
}

// ------------------------------------------------------------------------------------ the line cap

/// A notification larger than the 16 MiB line cap is skipped and named, and the stream goes on: the turn
/// ends and the next one runs.
#[test]
#[ignore = "runs the real codex (on PATH) against the scripted model; no login, no network"]
fn over_cap_line_mid_stream_is_skipped() {
    let codex = real_codex();
    let s = Scratch::new("overcap");
    let huge = "x".repeat(agent::jsonrpc_link::MAX_LINE + (1 << 20));
    let model = ScriptedModel::start(vec![Step::say(&huge).when_body_has("PROMPT-HUGE")]).expect("model");
    let home = s.dir("home");
    write_home(&home, &model, "");
    let project = s.dir("proj");
    write_project(&project, "");
    let mut peer = Peer::start(&codex, &home, &project, decline_all());
    let thread = start_thread(&mut peer, start_params(&project, DirTrust::Untrusted));

    // The agent's message is in an item notification and again in the turn's own end notification, so
    // both are over the cap: the link names each and does not end.
    let turn = send_turn(&mut peer, &thread.id, "PROMPT-HUGE");
    let thread_id = thread.id.clone();
    peer.wait_for(TURN_BUDGET, |p| {
        p.turn_status(&thread_id, &turn).is_some()
            || p.skipped.iter().any(|s| matches!(s, Skipped::TurnTooLarge { .. }))
    });
    peer.pump_for(Duration::from_secs(1));
    eprintln!("skipped: {:?}", peer.skipped);
    assert!(
        peer.skipped.iter().any(|s| matches!(
            s,
            Skipped::ItemTooLarge { method: m, item_id: Some(_), .. }
                if m == method::ITEM_COMPLETED
        )),
        "the over-long item notification was not named with its item: {:?}",
        peer.skipped
    );
    assert!(peer.closed.is_none(), "the link ended: {:?}", peer.closed);
    // However the turn's end reached us (whole, or skipped and named), the thread is idle again.
    assert!(
        peer.turn_status(&thread.id, &turn).is_some()
            || peer.skipped.iter().any(|s| matches!(
                s,
                Skipped::TurnTooLarge { method: m, thread_id: Some(t) }
                    if m == method::TURN_COMPLETED && *t == thread.id
            )),
        "the turn's end was neither read nor named: {:?}",
        peer.skipped
    );
    // The link is still usable: a turn on another thread runs to its end. (The first thread's history now
    // holds the huge message, which the next request to the stand-in would carry back over its own cap.)
    let other = start_thread(&mut peer, start_params(&project, DirTrust::Untrusted));
    assert_eq!(run_turn(&mut peer, &other.id, "after the huge one"), "completed");
    peer.close();
}

// ------------------------------------------------------------------------------------ tool approvals

/// A project tool server's tool that is not read-only reaches the client as a form for that server;
/// accepting it once runs the tool, declining it does not. The stand-in model answers under a slug Codex has
/// no catalog entry for, so the tool is offered as a plain function (code mode would hide it behind `exec`).
#[test]
#[ignore = "runs the real codex (on PATH) against the scripted model; no login, no network"]
fn tool_approval_is_a_form_for_the_server() {
    let codex = real_codex();
    for accept in [true, false] {
        let s = Scratch::new(if accept { "mcp-form-accept" } else { "mcp-form-decline" });
        let pid_file = s.path("tool-server.pid");
        let _guard = KillOnDrop(pid_file.clone());
        let model = ScriptedModel::start(vec![Step::call("t1", Some("mcp__eitri_local"), "ping", &json!({}))
            .when_body_has("PROMPT-PING")
            .unless_body_has("\"t1\"")])
        .expect("model");
        let home = s.dir("home");
        write_home_for(&home, &model, "eitri-plain-tools", "");
        let project = s.dir("proj");
        write_project(&project, &local_tool_server(&s, "eitri_local", &pid_file, false));
        let answer: Answer = Box::new(move |req| {
            if req.method == method::MCP_ELICITATION {
                let reply = if accept {
                    ElicitationResponse::accept_once()
                } else {
                    ElicitationResponse::decline()
                };
                Some(serde_json::to_value(reply).expect("json"))
            } else {
                Some(json!({"decision": "decline"}))
            }
        });
        let mut peer = Peer::start(&codex, &home, &project, answer);
        let thread = start_thread(&mut peer, start_params(&project, DirTrust::Trusted));
        assert_eq!(run_turn(&mut peer, &thread.id, "PROMPT-PING"), "completed");

        let forms: Vec<&Req> = peer
            .requests
            .iter()
            .filter(|r| r.method == method::MCP_ELICITATION)
            .collect();
        assert_eq!(forms.len(), 1, "expected one form, got {}", forms.len());
        let form = &forms[0].params;
        assert_eq!(json_str(form, "/serverName"), Some("eitri_local"));
        assert_eq!(json_str(form, "/mode"), Some("form"));
        assert_eq!(json_str(form, "/_meta/codex_approval_kind"), Some("mcp_tool_call"));
        assert!(
            json_str(form, "/message").is_some_and(|m| m.contains("ping")),
            "the form does not name the tool: {form}"
        );
        assert_eq!(json_str(form, "/threadId"), Some(thread.id.as_str()));
        let output = call_outputs(&model).remove("t1").unwrap_or_default();
        if accept {
            assert!(output.contains("pong"), "an accepted tool call did not run: {output}");
        } else {
            assert!(!output.contains("pong"), "a declined tool call ran: {output}");
            assert!(!output.is_empty(), "the model was not told the call was declined");
        }
        peer.close();
    }
}
