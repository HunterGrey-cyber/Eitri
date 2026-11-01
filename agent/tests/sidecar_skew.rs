//! A sidecar this build did not ship with, against a stand-in sidecar
//! (`tests/support/sidecar_standin.rs`) scripted through `EITRI_STANDIN_*`: what a handshake it did not
//! expect leaves running, and what makes a selected update fall back to the bundled sidecar. No real
//! `claude` runs here.
//!
//! Every stand-in is started through a small launcher that sets its script, since the tests share
//! this process's environment.
#![cfg(target_os = "linux")]

use agent::providers::claude_sidecar::{check_update_floor, SidecarVersion};
use agent::setting_sources::ProjectTrust;
use agent::{
    AgentDomainEvent, AgentProvider, ClaudeSidecarProvider, ContentKind, CreateSessionRequest, PermissionDecision,
    ProviderCapabilities, ResolvePermissionRequest, SendTurnRequest, StreamingPreference, BUNDLED_SIDECAR_VERSION,
    REQUIRED_CAPABILITIES,
};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// How long any one wait here may take before the test is called hung. Not a speed bound: every
/// wait ends as soon as what it waits for has happened.
const HANG: Duration = Duration::from_secs(30);

/// Set in the environment of the copy of this binary the `connect` tests start; nothing else sets it.
const HELPER: &str = "EITRI_SIDECAR_SKEW_HELPER";

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("sidecar-skew-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// A program at `path` that runs `before`, then the stand-in with `vars` in its environment.
fn launcher_at(path: &Path, vars: &[(&str, &str)], before: &str) {
    let mut script = String::from("#!/bin/sh\n[ \"$1\" = ready ] && exit 0\n");
    script.push_str(before);
    script.push('\n');
    for (name, value) in vars {
        script.push_str(&format!("export {name}={}\n", quoted(value)));
    }
    script.push_str(&format!(
        "exec {} \"$@\"\n",
        quoted(env!("CARGO_BIN_EXE_eitri-sidecar-standin"))
    ));
    agent::private_fs::write_private(path, script.as_bytes()).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    // A child forked by another test at the moment of the write holds the file open for writing until
    // it reaches its own exec, and until then this file does not run. The start under test must not
    // meet that, so it is met here.
    let started = Instant::now();
    while Command::new(path).arg("ready").status().is_err() {
        assert!(started.elapsed() < HANG, "the launcher never ran");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn launcher(dir: &Path, name: &str, vars: &[(&str, &str)]) -> PathBuf {
    let path = dir.join(name);
    launcher_at(&path, vars, "");
    path
}

fn instance() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// What a sidecar at `file` (`capabilities.json` at some tag) advertises, its conditional
/// capabilities aside, and its minor.
fn frozen(file: &Path) -> (Vec<String>, u32) {
    let text = std::fs::read_to_string(file).unwrap();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    let names = json["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry.get("when").is_none())
        .map(|entry| entry["name"].as_str().unwrap().to_string())
        .collect();
    (names, json["protocol"]["minor"].as_u64().unwrap() as u32)
}

fn handshake_fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/handshake")
}

/// What a sidecar of this build's own pin advertises, with one name taken out.
fn current_without(absent: &str) -> String {
    let (names, _) = frozen(&handshake_fixtures().join("3.24.json"));
    let kept: Vec<String> = names.into_iter().filter(|name| name != absent).collect();
    kept.join(" ")
}

fn current() -> String {
    current_without("")
}

fn open_session(provider: &ClaudeSidecarProvider) -> String {
    provider
        .create_session(CreateSessionRequest {
            cwd: "/".to_string(),
            streaming: StreamingPreference::Partial,
            project: ProjectTrust::Untrusted,
            client_tools: None,
        })
        .expect("the stand-in opens a session")
}

fn send(provider: &ClaudeSidecarProvider, session_id: &str, script: &str) {
    provider
        .send_turn(SendTurnRequest {
            session_id: session_id.to_string(),
            text: script.to_string(),
        })
        .expect("the turn is taken");
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

fn completed(events: &[AgentDomainEvent]) -> bool {
    events
        .iter()
        .any(|event| matches!(event, AgentDomainEvent::TurnCompleted { .. }))
}

fn text_of(events: &[AgentDomainEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            AgentDomainEvent::ContentDelta {
                kind: ContentKind::Text,
                text,
                ..
            } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// One whole turn on a new session.
fn a_turn_completes(provider: &ClaudeSidecarProvider) {
    let session = open_session(provider);
    send(provider, &session, "text m1 hello; end");
    let mut seen = Vec::new();
    pump_until(provider, &mut seen, "the turn's end", completed);
    assert_eq!(text_of(&seen), "hello");
}

fn fallback_line(start: &str) -> String {
    format!(
        "{start}, so this tab runs the bundled sidecar {BUNDLED_SIDECAR_VERSION}. To stop trying it: eitri sidecar use bundled"
    )
}

/// The provider is the bundled one, and says why.
fn assert_fell_back(provider: &ClaudeSidecarProvider, line: &str) {
    let info = provider.info();
    assert_eq!(
        info.protocol_minor, BUNDLED_MINOR,
        "the bundled stand-in serves this tab"
    );
    assert_eq!(info.sidecar_update, None);
    assert_eq!(
        info.startup_diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.starts_with("Sidecar update"))
            .collect::<Vec<_>>(),
        vec![line]
    );
}

/// The minors the stand-ins of the update tests answer with, so a test can see who serves a tab.
const BUNDLED_MINOR: u32 = 24;
const UPDATE_MINOR: &str = "25";

fn bundled(dir: &Path) -> PathBuf {
    let capabilities = current();
    launcher(
        dir,
        "bundled",
        &[
            ("EITRI_STANDIN_CAPABILITIES", capabilities.as_str()),
            ("EITRI_STANDIN_MINOR", "24"),
        ],
    )
}

// What a handshake may hold, on any sidecar.

#[test]
fn a_higher_minor_with_unknown_capability_arm_and_field_completes_a_turn_and_loses_nothing() {
    let dir = Scratch::new();
    let capabilities = format!("{} a_capability_from_a_later_minor", current());
    let future = launcher(
        &dir.0,
        "future",
        &[
            ("EITRI_STANDIN_CAPABILITIES", capabilities.as_str()),
            ("EITRI_STANDIN_MINOR", "99"),
            ("EITRI_STANDIN_UNKNOWN_WIRE", "1"),
        ],
    );
    let provider = ClaudeSidecarProvider::connect_to_program(&instance(), &future).unwrap();
    let present = launcher(&dir.0, "present", &[("EITRI_STANDIN_CAPABILITIES", current().as_str())]);
    let plain = ClaudeSidecarProvider::connect_to_program(&instance(), &present).unwrap();
    assert_eq!(
        provider.capabilities(),
        plain.capabilities(),
        "an unknown string changes nothing"
    );
    assert_eq!(provider.info().protocol_minor, 99);
    assert!(provider
        .info()
        .advertised_capabilities
        .iter()
        .any(|name| name == "a_capability_from_a_later_minor"));

    let session = open_session(&provider);
    send(
        &provider,
        &session,
        "text m1 before ; unknown-event ; text m1 after ; ask-unknown p1 ; text m2 answered ; end",
    );
    let mut seen = Vec::new();
    pump_until(&provider, &mut seen, "the card", |events| {
        events
            .iter()
            .any(|event| matches!(event, AgentDomainEvent::PermissionRequested { .. }))
    });
    let prompt = seen.iter().find_map(|event| match event {
        AgentDomainEvent::PermissionRequested { provider_prompt, .. } => provider_prompt.as_ref(),
        _ => None,
    });
    let prompt = prompt.expect("an unknown origin is a prompt of the CLI's");
    assert!(prompt.needs_its_own_card(), "{prompt:?}");
    provider
        .resolve_permission(ResolvePermissionRequest {
            session_id: session,
            permission_id: "p1".to_string(),
            decision: PermissionDecision::Allow,
        })
        .unwrap();
    pump_until(&provider, &mut seen, "the turn's end", completed);
    assert_eq!(text_of(&seen), "beforeafteranswered");
}

/// Each capability's flag, for those a flag reads.
fn flag_of(capability: &str, flags: &ProviderCapabilities) -> Option<bool> {
    Some(match capability {
        "resume_session" => flags.resume,
        "interrupt_turn" => flags.interrupt,
        "steer_turn" => flags.steer,
        "cli_turns" => flags.cli_turns,
        "human_origin" => flags.human_origin,
        "background_tasks" => flags.background_tasks,
        "workflow_progress" => flags.workflow_progress,
        "task_control" => flags.task_control,
        "subagent_frames" => flags.subagent_frames,
        "session_notices" => flags.session_notices,
        "conversation_reset" => flags.conversation_reset,
        "structured_questions_v1" => flags.structured_questions_v1,
        "client_tools" => flags.client_tools,
        "set_model" | "list_models" | "apply_effort" => flags.direct_settings,
        _ => return None,
    })
}

#[test]
fn each_optional_capability_removed_leaves_a_running_tab_and_reads_as_absent() {
    let dir = Scratch::new();
    let (names, _) = frozen(&handshake_fixtures().join("3.24.json"));
    // `steer_turn` is conditional on a real sidecar, and is one more name to take out here.
    let mut optional: Vec<String> = names
        .into_iter()
        .filter(|name| !REQUIRED_CAPABILITIES.contains(&name.as_str()))
        .collect();
    optional.push("steer_turn".to_string());

    let all = format!("{} steer_turn", current());
    let all = launcher(&dir.0, "all", &[("EITRI_STANDIN_CAPABILITIES", all.as_str())]);
    let full = ClaudeSidecarProvider::connect_to_program(&instance(), &all).unwrap();
    for name in &optional {
        if let Some(flag) = flag_of(name, &full.capabilities()) {
            assert!(flag, "{name} reads as present when it is");
        }
    }

    for name in &optional {
        let capabilities = format!("{} steer_turn", current())
            .split(' ')
            .filter(|offered| offered != name)
            .collect::<Vec<_>>()
            .join(" ");
        let program = launcher(
            &dir.0,
            &format!("without-{name}"),
            &[("EITRI_STANDIN_CAPABILITIES", capabilities.as_str())],
        );
        let provider = ClaudeSidecarProvider::connect_to_program(&instance(), &program)
            .unwrap_or_else(|e| panic!("without {name}: {e}"));
        assert!(!provider.info().advertised_capabilities.contains(name));
        if let Some(flag) = flag_of(name, &provider.capabilities()) {
            assert!(!flag, "{name} reads as absent");
        }
        open_session(&provider);
    }
}

#[test]
fn a_required_capability_missing_on_a_plain_connect_still_connects() {
    let dir = Scratch::new();
    for name in REQUIRED_CAPABILITIES {
        let capabilities = current_without(name);
        let program = launcher(
            &dir.0,
            &format!("without-{name}"),
            &[("EITRI_STANDIN_CAPABILITIES", capabilities.as_str())],
        );
        let provider = ClaudeSidecarProvider::connect_to_program(&instance(), &program)
            .unwrap_or_else(|e| panic!("without {name}: {e}"));
        a_turn_completes(&provider);
    }
}

#[test]
fn the_frozen_handshakes_run_on_this_build_and_pass_the_floor() {
    let dir = Scratch::new();
    let mut seen = 0;
    for entry in std::fs::read_dir(handshake_fixtures()).unwrap() {
        let file = entry.unwrap().path();
        let (names, minor) = frozen(&file);
        assert_eq!(
            file.file_stem().unwrap().to_str().unwrap(),
            format!("3.{minor}"),
            "a fixture is named for its minor"
        );
        let capabilities = names.join(" ");
        let minor_text = minor.to_string();
        let program = launcher(
            &dir.0,
            &format!("frozen-{minor}"),
            &[
                ("EITRI_STANDIN_CAPABILITIES", capabilities.as_str()),
                ("EITRI_STANDIN_MINOR", minor_text.as_str()),
            ],
        );
        let provider = ClaudeSidecarProvider::connect_to_program(&instance(), &program).unwrap();
        assert_eq!(provider.info().protocol_minor, minor);
        assert_eq!(check_update_floor(&names), Ok(()), "{}", file.display());
        a_turn_completes(&provider);
        seen += 1;
    }
    assert!(seen >= 2, "the fixtures are found");
}

/// A move has committed by the time `adopt` runs, so a refusal there would end the chat.
#[test]
fn a_sidecar_lacking_a_required_capability_is_adopted_and_its_turn_goes_on() {
    let dir = Scratch::new();
    for name in REQUIRED_CAPABILITIES {
        let capabilities = current_without(name);
        let program = launcher(
            &dir.0,
            &format!("without-{name}"),
            &[("EITRI_STANDIN_CAPABILITIES", capabilities.as_str())],
        );
        let source = ClaudeSidecarProvider::connect_to_program(&instance(), &program).unwrap();
        let session = open_session(&source);
        let mut seen = Vec::new();
        pump_until(&source, &mut seen, "the session", |events| {
            events
                .iter()
                .any(|event| matches!(event, AgentDomainEvent::SessionOpened { .. }))
        });
        send(&source, &session, "text m1 before ; sleep 300 ; text m1 after ; end");
        pump_until(&source, &mut seen, "the first words", |events| {
            text_of(events).contains("before")
        });
        let (handoff, fds) = source.freeze_for_move().expect("the source hands the sidecar over");
        let destination = ClaudeSidecarProvider::adopt(handoff, fds).expect("the destination takes it over");
        destination.commit().expect("the destination commits");
        source.forget_for_move();
        let mut after = Vec::new();
        pump_until(&destination, &mut after, "the turn's end", completed);
        assert!(text_of(&after).contains("after"), "{after:#?}");
    }
}

// A selected update, tried and given up.

#[test]
fn an_update_that_answers_runs_the_tab_and_says_which_it_is() {
    let dir = Scratch::new();
    let capabilities = current();
    let update = launcher(
        &dir.0,
        "update",
        &[
            ("EITRI_STANDIN_CAPABILITIES", capabilities.as_str()),
            ("EITRI_STANDIN_MINOR", UPDATE_MINOR),
        ],
    );
    let provider =
        ClaudeSidecarProvider::connect_trying_update(&instance(), &update, "1.0.0", &bundled(&dir.0)).unwrap();
    assert_eq!(provider.info().protocol_minor, 25);
    assert_eq!(provider.info().sidecar_update.as_deref(), Some("1.0.0"));
    assert!(
        !provider
            .info()
            .startup_diagnostics
            .iter()
            .any(|line| line.starts_with("Sidecar update")),
        "{:?}",
        provider.info().startup_diagnostics
    );
    a_turn_completes(&provider);
}

#[test]
fn a_plain_start_names_no_update() {
    let dir = Scratch::new();
    let provider = ClaudeSidecarProvider::connect_to_program(&instance(), &bundled(&dir.0)).unwrap();
    assert_eq!(provider.info().sidecar_update, None);
}

#[test]
fn a_fallback_tab_works_like_any_other() {
    let dir = Scratch::new();
    let update = launcher(&dir.0, "update", &[("EITRI_STANDIN_REFUSE_BIND", "gone\n")]);
    let provider =
        ClaudeSidecarProvider::connect_trying_update(&instance(), &update, "1.0.1", &bundled(&dir.0)).unwrap();
    assert_fell_back(&provider, &fallback_line("Sidecar update 1.0.1 did not start (gone)"));
    a_turn_completes(&provider);
}

/// The failing stand-in in each test names a version of its own: the set of failed versions is
/// this process's, and every test here shares it.
#[test]
fn each_required_capability_missing_falls_back_and_names_it() {
    let dir = Scratch::new();
    let bundled = bundled(&dir.0);
    for (index, name) in REQUIRED_CAPABILITIES.iter().enumerate() {
        let capabilities = current_without(name);
        let update = launcher(
            &dir.0,
            &format!("without-{name}"),
            &[
                ("EITRI_STANDIN_CAPABILITIES", capabilities.as_str()),
                ("EITRI_STANDIN_MINOR", UPDATE_MINOR),
            ],
        );
        let version = format!("1.1.{index}");
        let provider = ClaudeSidecarProvider::connect_trying_update(&instance(), &update, &version, &bundled).unwrap();
        assert_fell_back(
            &provider,
            &fallback_line(&format!("Sidecar update {version} lacks {name}")),
        );
    }
}

#[test]
fn a_refusal_of_the_handshake_falls_back_in_the_sidecars_words() {
    let dir = Scratch::new();
    let words = "client protocol_major 3 is incompatible with this sidecar's 4";
    let update = launcher(&dir.0, "update", &[("EITRI_STANDIN_INCOMPATIBLE", words)]);
    let provider =
        ClaudeSidecarProvider::connect_trying_update(&instance(), &update, "1.2.0", &bundled(&dir.0)).unwrap();
    assert_fell_back(
        &provider,
        &fallback_line(&format!("Sidecar update 1.2.0 refused this Eitri ({words})")),
    );
}

/// A sidecar that answers another major instead of refusing it: not conforming, and refused here.
#[test]
fn a_handshake_answering_another_major_falls_back() {
    let dir = Scratch::new();
    let update = launcher(&dir.0, "update", &[("EITRI_STANDIN_MAJOR", "4")]);
    let provider =
        ClaudeSidecarProvider::connect_trying_update(&instance(), &update, "1.2.1", &bundled(&dir.0)).unwrap();
    let line = provider
        .info()
        .startup_diagnostics
        .into_iter()
        .find(|line| line.starts_with("Sidecar update"))
        .expect("the tab says so");
    assert!(line.starts_with("Sidecar update 1.2.1 refused this Eitri ("), "{line}");
    assert!(line.contains("protocol major 4"), "{line}");
    assert_fell_back(&provider, &line);
}

/// The lines a real refusal printed, captured in the panel's own fixture: the account line first,
/// which is not the cause.
fn real_refusal() -> (String, String) {
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent-ui/web/src/fixtures/problems/cli-out-of-range.txt");
    let text = std::fs::read_to_string(fixture).unwrap();
    let (_, printed) = text.split_once("its stderr said:\n").unwrap();
    let refusal = printed
        .lines()
        .find(|line| line.starts_with("refusing to start"))
        .unwrap()
        .to_string();
    assert!(
        !printed.starts_with("refusing to start"),
        "the fixture opens with its account line"
    );
    (printed.to_string(), refusal)
}

#[test]
fn a_real_refusal_is_quoted_not_the_account_line_before_it() {
    let dir = Scratch::new();
    let (printed, refusal) = real_refusal();
    let update = launcher(&dir.0, "update", &[("EITRI_STANDIN_REFUSE_BIND", printed.as_str())]);
    let provider =
        ClaudeSidecarProvider::connect_trying_update(&instance(), &update, "1.3.0", &bundled(&dir.0)).unwrap();
    assert_fell_back(
        &provider,
        &fallback_line(&format!("Sidecar update 1.3.0 did not start ({refusal})")),
    );
}

#[test]
fn a_crash_is_quoted_by_its_last_line() {
    let dir = Scratch::new();
    let update = launcher(
        &dir.0,
        "update",
        &[("EITRI_STANDIN_REFUSE_BIND", "an account line\nthe last words\n\n")],
    );
    let provider =
        ClaudeSidecarProvider::connect_trying_update(&instance(), &update, "1.3.1", &bundled(&dir.0)).unwrap();
    assert_fell_back(
        &provider,
        &fallback_line("Sidecar update 1.3.1 did not start (the last words)"),
    );
}

#[test]
fn a_silent_exit_is_reported_as_one() {
    let dir = Scratch::new();
    let update = launcher(&dir.0, "update", &[("EITRI_STANDIN_REFUSE_BIND", "")]);
    let provider =
        ClaudeSidecarProvider::connect_trying_update(&instance(), &update, "1.3.2", &bundled(&dir.0)).unwrap();
    let diagnostics = provider.info().startup_diagnostics;
    let line = diagnostics
        .iter()
        .find(|line| line.starts_with("Sidecar update"))
        .unwrap();
    assert!(
        line.starts_with("Sidecar update 1.3.2 did not start (exited with "),
        "{line}"
    );
    assert_fell_back(&provider, line);
}

#[test]
fn an_update_that_never_binds_is_reported_as_no_answer() {
    let dir = Scratch::new();
    let update = launcher(&dir.0, "update", &[("EITRI_STANDIN_NEVER_BIND", "1")]);
    let provider =
        ClaudeSidecarProvider::connect_trying_update(&instance(), &update, "1.3.3", &bundled(&dir.0)).unwrap();
    assert_fell_back(
        &provider,
        &fallback_line("Sidecar update 1.3.3 did not start (no answer in 15 s)"),
    );
}

#[test]
fn an_update_that_stalls_after_binding_is_reported_as_no_answer() {
    let dir = Scratch::new();
    let update = launcher(&dir.0, "update", &[("EITRI_STANDIN_STALL_HANDSHAKE", "1")]);
    let provider =
        ClaudeSidecarProvider::connect_trying_update(&instance(), &update, "1.3.4", &bundled(&dir.0)).unwrap();
    assert_fell_back(
        &provider,
        &fallback_line("Sidecar update 1.3.4 did not start (no answer in 10 s)"),
    );
}

/// Nothing is persisted, so a new process tries again; this one does not, and says so on every tab.
/// The line a real sidecar opens with: it names an account, and says nothing about what went wrong.
const ACCOUNT_LINE: &str = "claude account: work (/home/u/.claude-work)";

/// A launcher that prints [`ACCOUNT_LINE`] and then runs the stand-in as `vars` script it.
fn account_line_launcher(dir: &Path, vars: &[(&str, &str)]) -> PathBuf {
    let path = dir.join("update");
    launcher_at(&path, vars, &format!("echo {} >&2", quoted(ACCOUNT_LINE)));
    path
}

#[test]
fn an_account_line_alone_is_not_why_a_sidecar_exited() {
    let dir = Scratch::new();
    let update = launcher(
        &dir.0,
        "update",
        &[("EITRI_STANDIN_REFUSE_BIND", &format!("{ACCOUNT_LINE}\n"))],
    );
    let provider =
        ClaudeSidecarProvider::connect_trying_update(&instance(), &update, "1.3.6", &bundled(&dir.0)).unwrap();
    let diagnostics = provider.info().startup_diagnostics;
    let line = diagnostics
        .iter()
        .find(|line| line.starts_with("Sidecar update"))
        .unwrap();
    assert!(
        line.starts_with("Sidecar update 1.3.6 did not start (exited with "),
        "{line}"
    );
    assert_fell_back(&provider, line);
}

#[test]
fn an_account_line_alone_is_not_why_a_sidecar_never_bound() {
    let dir = Scratch::new();
    let update = account_line_launcher(&dir.0, &[("EITRI_STANDIN_NEVER_BIND", "1")]);
    let provider =
        ClaudeSidecarProvider::connect_trying_update(&instance(), &update, "1.3.7", &bundled(&dir.0)).unwrap();
    assert_fell_back(
        &provider,
        &fallback_line("Sidecar update 1.3.7 did not start (no answer in 15 s)"),
    );
}

#[test]
fn an_account_line_alone_is_not_why_a_handshake_stalled() {
    let dir = Scratch::new();
    let update = account_line_launcher(&dir.0, &[("EITRI_STANDIN_STALL_HANDSHAKE", "1")]);
    let provider =
        ClaudeSidecarProvider::connect_trying_update(&instance(), &update, "1.3.8", &bundled(&dir.0)).unwrap();
    assert_fell_back(
        &provider,
        &fallback_line("Sidecar update 1.3.8 did not start (no answer in 10 s)"),
    );
}

/// What a fallback line quotes lands in the band's title, so a sidecar's long line is cut there.
#[test]
fn a_long_last_line_is_cut_in_the_fallback_line() {
    let dir = Scratch::new();
    let long = "x".repeat(60_000);
    let update = launcher(&dir.0, "update", &[("EITRI_STANDIN_REFUSE_BIND", long.as_str())]);
    let provider =
        ClaudeSidecarProvider::connect_trying_update(&instance(), &update, "1.3.9", &bundled(&dir.0)).unwrap();
    let diagnostics = provider.info().startup_diagnostics;
    let line = diagnostics
        .iter()
        .find(|line| line.starts_with("Sidecar update"))
        .unwrap();
    assert!(line.starts_with("Sidecar update 1.3.9 did not start (xxx"), "{line}");
    assert!(line.contains("x…)"), "{line}");
    assert!(line.chars().count() < 1000, "{}", line.chars().count());
}

#[test]
fn a_long_refusal_is_cut_in_the_fallback_line() {
    let dir = Scratch::new();
    let long = "y".repeat(2000);
    let update = launcher(&dir.0, "update", &[("EITRI_STANDIN_INCOMPATIBLE", long.as_str())]);
    let provider =
        ClaudeSidecarProvider::connect_trying_update(&instance(), &update, "1.3.10", &bundled(&dir.0)).unwrap();
    let diagnostics = provider.info().startup_diagnostics;
    let line = diagnostics
        .iter()
        .find(|line| line.starts_with("Sidecar update"))
        .unwrap();
    assert!(
        line.starts_with("Sidecar update 1.3.10 refused this Eitri (yyy"),
        "{line}"
    );
    assert!(line.contains("y…)"), "{line}");
    assert!(line.chars().count() < 1000, "{}", line.chars().count());
}

#[test]
fn an_update_that_failed_is_not_started_again_and_every_tab_says_so() {
    let dir = Scratch::new();
    let starts = dir.0.join("starts");
    let update = dir.0.join("update");
    launcher_at(
        &update,
        &[("EITRI_STANDIN_REFUSE_BIND", "gone\n")],
        &format!("echo started >> {}", quoted(starts.to_str().unwrap())),
    );
    let bundled = bundled(&dir.0);
    let first = ClaudeSidecarProvider::connect_trying_update(&instance(), &update, "1.4.0", &bundled).unwrap();
    assert_fell_back(&first, &fallback_line("Sidecar update 1.4.0 did not start (gone)"));
    for _ in 0..2 {
        let next = ClaudeSidecarProvider::connect_trying_update(&instance(), &update, "1.4.0", &bundled).unwrap();
        assert_fell_back(
            &next,
            &fallback_line("Sidecar update 1.4.0 did not start earlier in this window"),
        );
    }
    assert_eq!(std::fs::read_to_string(&starts).unwrap().lines().count(), 1);
}

#[test]
fn update_and_bundled_both_failing_fail_the_tab_with_the_bundled_error_first() {
    let dir = Scratch::new();
    let update = launcher(&dir.0, "update", &[("EITRI_STANDIN_REFUSE_BIND", "update gone\n")]);
    let refusal = "refusing to start: host claude CLI version 3.0.0 is outside this sidecar's supported range\n";
    let bundled = launcher(&dir.0, "bundled", &[("EITRI_STANDIN_REFUSE_BIND", refusal)]);
    let error = ClaudeSidecarProvider::connect_trying_update(&instance(), &update, "1.5.0", &bundled)
        .err()
        .expect("nothing runs");
    let text = error.to_string();
    let (first, last) = text.rsplit_once('\n').unwrap();
    assert!(
        first.contains("before binding") && first.contains(refusal.trim_end()),
        "{first}"
    );
    assert_eq!(last, "Sidecar update 1.5.0 also did not start: update gone");
}

#[test]
fn an_update_newer_than_a_missing_bundled_runs() {
    let dir = Scratch::new();
    let update = launcher(&dir.0, "update", &[("EITRI_STANDIN_CAPABILITIES", current().as_str())]);
    let provider = ClaudeSidecarProvider::connect_trying_update(
        &instance(),
        &update,
        "1.6.0",
        &dir.0.join("the-bundled-sidecar-was-never-built"),
    )
    .unwrap();
    assert_eq!(provider.info().sidecar_update.as_deref(), Some("1.6.0"));
}

#[test]
fn an_update_that_failed_with_nothing_bundled_leaves_both_errors() {
    let dir = Scratch::new();
    let update = launcher(&dir.0, "update", &[("EITRI_STANDIN_REFUSE_BIND", "gone\n")]);
    let error = ClaudeSidecarProvider::connect_trying_update(
        &instance(),
        &update,
        "1.6.1",
        &dir.0.join("the-bundled-sidecar-was-never-built"),
    )
    .err()
    .expect("nothing runs");
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    assert!(
        error
            .to_string()
            .ends_with("\nSidecar update 1.6.1 also did not start: gone"),
        "{error}"
    );
}

#[test]
fn connecting_for_a_route_starts_the_program_named() {
    let dir = Scratch::new();
    let provider = ClaudeSidecarProvider::connect_to_program_in(&instance(), &bundled(&dir.0), &dir.0).unwrap();
    a_turn_completes(&provider);
}

/// The `connect` tests below go through the comparison with the bundled version.
#[test]
fn the_update_on_disk_in_these_tests_stays_newer_than_bundled() {
    let bundled = SidecarVersion::parse(BUNDLED_SIDECAR_VERSION).unwrap();
    assert!(SidecarVersion::parse("1.7.0").unwrap() > bundled);
}

// `connect` itself: a copy of this binary, standing as an installed `eitri` does, with `selected` and
// `updates/` on disk.

/// One line of the helper's report.
fn field<'a>(output: &'a str, key: &str) -> &'a str {
    output
        .lines()
        .find_map(|line| line.strip_prefix(key))
        .unwrap_or_else(|| panic!("no {key} in: {output}"))
}

/// Runs in the copy, and only there.
#[test]
fn connect_helper() {
    if std::env::var_os(HELPER).is_none() {
        return;
    }
    match ClaudeSidecarProvider::connect(&instance()) {
        Ok(provider) => {
            let info = provider.info();
            println!("MINOR={}", info.protocol_minor);
            println!("UPDATE={}", info.sidecar_update.as_deref().unwrap_or("none"));
            for line in info.startup_diagnostics {
                println!("DIAGNOSTIC={line}");
            }
        }
        Err(error) => println!("ERROR={error}"),
    }
}

struct Installed {
    dir: Scratch,
    exe: PathBuf,
    sidecar_root: PathBuf,
}

impl Installed {
    /// A copy of this test binary with a bundled stand-in as its sibling, `eitri-setup` beside it.
    fn new() -> Self {
        let dir = Scratch::new();
        let lib = dir.0.join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        let exe = lib.join("eitri");
        std::fs::hard_link(std::env::current_exe().unwrap(), &exe).unwrap();
        std::fs::write(lib.join("eitri-setup"), b"#!/bin/sh\n").unwrap();
        let capabilities = current();
        launcher_at(
            &lib.join("verdandi-claude-sidecar"),
            &[
                ("EITRI_STANDIN_CAPABILITIES", capabilities.as_str()),
                ("EITRI_STANDIN_MINOR", "24"),
            ],
            "",
        );
        let sidecar_root = dir.0.join("data/eitri/sidecar");
        std::fs::create_dir_all(&sidecar_root).unwrap();
        Installed { dir, exe, sidecar_root }
    }

    fn build_update(&self, version: &str, vars: &[(&str, &str)]) {
        let directory = self.sidecar_root.join("updates").join(version);
        std::fs::create_dir_all(&directory).unwrap();
        launcher_at(&directory.join("verdandi-claude-sidecar"), vars, "");
        std::fs::write(directory.join("BUILD"), b"built\n").unwrap();
    }

    fn select(&self, contents: &[u8]) {
        std::fs::write(self.sidecar_root.join("selected"), contents).unwrap();
    }

    /// Everything the helper printed, `EITRI_SIDECAR_BINARY` set to `named` if there is one.
    fn connect_naming(&self, named: Option<&Path>) -> String {
        let mut command = Command::new(&self.exe);
        command
            .args(["--exact", "connect_helper", "--nocapture"])
            .env(HELPER, "1")
            .env("HOME", self.dir.0.join("home"))
            .env("XDG_DATA_HOME", self.dir.0.join("data"))
            .env_remove("EITRI_SIDECAR_BINARY")
            .env_remove("EITRI_VERDANDI_CHECKOUT");
        if let Some(named) = named {
            command.env("EITRI_SIDECAR_BINARY", named);
        }
        String::from_utf8_lossy(&command.output().unwrap().stdout).into_owned()
    }

    fn connect(&self) -> String {
        self.connect_naming(None)
    }
}

#[test]
fn connect_runs_the_selected_update_ahead_of_the_sibling() {
    let installed = Installed::new();
    let capabilities = current();
    installed.build_update(
        "1.7.0",
        &[
            ("EITRI_STANDIN_CAPABILITIES", capabilities.as_str()),
            ("EITRI_STANDIN_MINOR", UPDATE_MINOR),
        ],
    );
    installed.select(b"version=1.7.0\n");
    let output = installed.connect();
    assert_eq!(field(&output, "MINOR="), "25", "{output}");
    assert_eq!(field(&output, "UPDATE="), "1.7.0", "{output}");
}

#[test]
fn connect_with_no_selected_file_runs_the_sibling_and_says_nothing() {
    let installed = Installed::new();
    let capabilities = current();
    installed.build_update(
        "1.7.0",
        &[
            ("EITRI_STANDIN_CAPABILITIES", capabilities.as_str()),
            ("EITRI_STANDIN_MINOR", UPDATE_MINOR),
        ],
    );
    let output = installed.connect();
    assert_eq!(field(&output, "MINOR="), "24", "{output}");
    assert_eq!(field(&output, "UPDATE="), "none", "{output}");
    assert!(!output.contains("DIAGNOSTIC=Sidecar update"), "{output}");
}

#[test]
fn connect_with_junk_selected_runs_the_sibling_and_says_so() {
    let installed = Installed::new();
    for contents in [
        b"junk\n".to_vec(),
        b"version=../x\n".to_vec(),
        b"version=1.7.0\nversion=1.7.1\n".to_vec(),
        vec![b'x'; 1024 * 1024],
    ] {
        installed.select(&contents);
        let output = installed.connect();
        assert_eq!(field(&output, "MINOR="), "24", "{output}");
        assert_eq!(field(&output, "UPDATE="), "none", "{output}");
        let line = field(&output, "DIAGNOSTIC=Sidecar update: ");
        assert!(line.ends_with(fallback_line("").trim_start_matches(", ")), "{line}");
    }
}

/// `BUILD` is written last, so a directory without it is one a build did not finish.
#[test]
fn connect_with_a_half_written_update_runs_the_sibling_and_says_so() {
    let installed = Installed::new();
    installed.build_update("1.7.0", &[]);
    std::fs::remove_file(installed.sidecar_root.join("updates/1.7.0/BUILD")).unwrap();
    installed.select(b"version=1.7.0\n");
    let output = installed.connect();
    assert_eq!(field(&output, "MINOR="), "24", "{output}");
    let line = field(&output, "DIAGNOSTIC=Sidecar update: ");
    assert!(line.contains("1.7.0") && line.contains("BUILD"), "{line}");
}

/// A named sidecar is what a person asked to see, so nothing falls back to it or from it.
#[test]
fn connect_with_a_named_binary_leaves_the_update_alone() {
    let installed = Installed::new();
    installed.build_update("1.7.0", &[("EITRI_STANDIN_MINOR", UPDATE_MINOR)]);
    installed.select(b"version=1.7.0\n");
    let named = installed.dir.0.join("named");
    launcher_at(&named, &[("EITRI_STANDIN_MINOR", "30")], "");
    let output = installed.connect_naming(Some(&named));
    assert_eq!(field(&output, "MINOR="), "30", "{output}");
    assert_eq!(field(&output, "UPDATE="), "none", "{output}");
}
