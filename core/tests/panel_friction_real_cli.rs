//! Panel interactions against a real sidecar and real `claude`. Ignored by default because
//! these tests spawn real sessions and bill the account they run as.
//!
//! **Run only on the TEST profile, through the resolved binary, with a scratch state home:**
//!
//! ```sh
//! claude --version    # record the build
//! XDG_STATE_HOME=/tmp/nv-friction-real-state \
//!     cargo test -p eitri-core --test panel_friction_real_cli -- --ignored --nocapture --test-threads=1
//! ```
//!
//! The one legacy arm (`--features legacy-backend`) spawns `agent-hook`, a binary of the `agent`
//! package that only a build with the legacy feature produces, and `cargo test -p eitri-core` does not
//! build another package's binaries: build it first (`cargo build -p agent --features legacy-backend
//! --bin agent-hook`) or name `-p agent` in the same `cargo test` invocation, or that arm fails with
//! "agent-hook binary not found" before any turn runs.

use eitri_core::agent_backend::{AgentBackend, BackendKind};
use eitri_core::agent_bridge::SessionModeChoice;
use eitri_core::tab_set::{SendNow, TabBackend, TabSet};
use std::time::{Duration, Instant};

fn project(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("nv-friction-real-{label}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("main.rs"), "fn marmalade() {}\n").unwrap();
    dir.canonicalize().unwrap()
}

fn live_set(dir: &std::path::Path) -> (TabSet, eitri_core::tabs::TabId) {
    live_set_on(BackendKind::Sidecar, dir)
}

fn live_set_on(kind: BackendKind, dir: &std::path::Path) -> (TabSet, eitri_core::tabs::TabId) {
    let mut set = TabSet::new(kind, SessionModeChoice::Auto);
    let tab = set.active();
    // Trusted: this measures the real CLI with the project tiers loaded, as it always has.
    let backend = AgentBackend::start(kind, dir, None, agent::setting_sources::ProjectTrust::Trusted)
        .map_err(|e| e.message)
        .expect("a sidecar session starts; is this running under a test-account wrapper?");
    set.get_mut(tab).unwrap().set_backend(TabBackend::Live(backend));
    (set, tab)
}

fn transcript(set: &TabSet, tab: eitri_core::tabs::TabId) -> String {
    let projection = set.get(tab).unwrap().live().unwrap().projection();
    projection
        .transcript
        .iter()
        .map(|m| m.text.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

fn pump_until(
    set: &mut TabSet,
    dir: &std::path::Path,
    what: &str,
    mut done: impl FnMut(&mut TabSet, Vec<eitri_core::tabs::TabId>) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(240);
    loop {
        let ended = set.pump(dir, true).turn_ended;
        if done(set, ended) {
            return;
        }
        if Instant::now() >= deadline {
            let tab = set.active();
            // Asked before the projection is taken: `turn_running` takes it too, and the guard is not
            // re-entrant.
            let running = set.get(tab).map(|t| t.turn_running());
            let state = set.get(tab).and_then(|t| t.live()).map(|b| {
                let p = b.projection();
                format!(
                    "running={running:?} pending={} tools={:?} transcript={:?}",
                    p.pending_permissions.len(),
                    p.tool_calls
                        .iter()
                        .map(|c| (c.name.clone(), c.input.clone()))
                        .collect::<Vec<_>>(),
                    p.transcript.iter().map(|m| m.text.clone()).collect::<Vec<_>>()
                )
            });
            panic!("timed out: {what}; {state:?}");
        }
        std::thread::sleep(Duration::from_millis(33));
    }
}

fn shut_down(set: &mut TabSet) {
    for tab in set.take_all() {
        if let Some(retired) = tab.retired {
            retired.into_backend().shutdown();
        }
    }
}

/// A queued message goes out after `Ctrl+Enter` interrupts the running turn.
#[test]
#[ignore = "real Claude; run under a test-account wrapper, see the module doc"]
fn a_queued_message_goes_out_after_an_interrupt() {
    let dir = project("queue");
    let (mut set, tab) = live_set(&dir);
    set.get_mut(tab)
        .unwrap()
        .live_mut()
        .unwrap()
        .send_turn(
            "Count slowly from 1 to 300, one number per line, with no other text.",
            "count",
            agent::PromptOrigin::Composed,
        )
        .map_err(|e| e.message)
        .unwrap();
    pump_until(&mut set, &dir, "the turn to start", |set, _| {
        set.get(tab).unwrap().turn_running()
    });
    set.queue_message(
        tab,
        "Now reply with exactly QUEUED-OK and nothing else.",
        "Now reply with exactly QUEUED-OK and nothing else.".into(),
        1,
        true,
        agent::PromptOrigin::Composed,
    )
    .unwrap();
    match set.send_now(tab, "", String::new(), 2).unwrap() {
        SendNow::Interrupting(outcome) => outcome.map_err(|e| e.message).unwrap(),
        SendNow::Flushed(_) => panic!("the turn was running"),
    };
    let mut flushed = false;
    pump_until(&mut set, &dir, "the queue to go out and be answered", |set, ended| {
        if ended.contains(&tab) && !flushed {
            let flush = set
                .flush_queue(tab)
                .expect("the queue flushes when the interrupted turn ends");
            flush.outcome.map_err(|e| e.message).unwrap();
            flushed = true;
        }
        flushed && !set.get(tab).unwrap().turn_running() && transcript(set, tab).contains("QUEUED-OK")
    });
    shut_down(&mut set);
}

/// A project rule answers matching Bash without a card; an option that writes still cards.
///
/// A `git log *` probe would pass even without a rule because the classifier already allows
/// read-only `git log`. `uname` is refused for the replaceable not-on-the-read-only-list reason,
/// so only a rule answers it. `git log --output=` checks that a writing option still cards.
#[test]
#[ignore = "real Claude; run under a test-account wrapper, see the module doc"]
fn a_rule_answers_its_command_and_git_log_output_still_cards() {
    let dir = project("rules");
    let (mut set, tab) = live_set(&dir);
    set.set_rules(
        agent::PrefixRules::default()
            .with(agent::PrefixRule::parse("Bash(uname *)").unwrap())
            .with(agent::PrefixRule::parse("Bash(git log *)").unwrap()),
    );
    set.get_mut(tab)
        .unwrap()
        .live_mut()
        .unwrap()
        .send_turn(
            "Use the Bash tool to run `uname -s` and tell me its output in one word.",
            "uname",
            agent::PromptOrigin::Composed,
        )
        .map_err(|e| e.message)
        .unwrap();
    pump_until(&mut set, &dir, "the uname turn", |set, _| {
        assert_eq!(
            set.get(tab).unwrap().attention.attention().pending,
            0,
            "a rule answered it: no card"
        );
        !set.get(tab).unwrap().turn_running() && transcript(set, tab).contains("Linux")
    });
    // Not vacuous (seen in a GUI pass, 2026-09-25): the first run of this test passed its first
    // half with NO Bash call at all -- `disallowed_tools_for(Auto)` removes `Bash` from the model's
    // tools, so it answered "Linux" from its environment info and nothing reached the gate. A rule
    // can only be shown to answer a call that was made.
    let ran_bash = set
        .get(tab)
        .unwrap()
        .live()
        .unwrap()
        .projection()
        .tool_calls
        .iter()
        .any(|call| call.name == "Bash" && call.input["command"].as_str().is_some_and(|c| c.contains("uname")));
    // Auto offers Bash, so an inconclusive probe here means the model chose not to call it.
    assert!(
        ran_bash,
        "no Bash call reached the gate, so a project rule was not shown answering anything \
         (check agent::disallowed_tools() still offers Bash)"
    );

    if cli_runs_auto(&set, tab) {
        the_cli_decides(
            &mut set,
            &dir,
            tab,
            "Use the Bash tool to run exactly this command, unchanged: git log --output=friction-probe.txt -1",
            "git log --output=friction-probe.txt",
        );
        the_cli_decides(
            &mut set,
            &dir,
            tab,
            "Use the Bash tool to run exactly this command, unchanged: uname -a > uname-probe.txt",
            "uname -a > uname-probe.txt",
        );
        shut_down(&mut set);
        return;
    }
    deny_the_next_card(
        &mut set,
        &dir,
        tab,
        "Use the Bash tool to run exactly this command, unchanged: git log --output=friction-probe.txt -1",
        "git log output",
    );
    assert!(!dir.join("friction-probe.txt").exists(), "denied: nothing was written");

    // A redirect is checked before any rule (rules replace only the two not-on-a-list reasons), so
    // the `uname *` rule must not answer `uname -a > file`.
    deny_the_next_card(
        &mut set,
        &dir,
        tab,
        "Use the Bash tool to run exactly this command, unchanged: uname -a > uname-probe.txt",
        "uname redirect",
    );
    assert!(!dir.join("uname-probe.txt").exists(), "denied: nothing was written");
    shut_down(&mut set);
}

/// Sends `prompt`, waits for exactly one card, denies it, and waits for the turn to end.
fn deny_the_next_card(
    set: &mut TabSet,
    dir: &std::path::Path,
    tab: eitri_core::tabs::TabId,
    prompt: &str,
    label: &str,
) -> agent::PermissionRequestRecord {
    set.get_mut(tab)
        .unwrap()
        .live_mut()
        .unwrap()
        .send_turn(prompt, label, agent::PromptOrigin::Composed)
        .map_err(|e| e.message)
        .unwrap();
    pump_until(set, dir, label, |set, _| {
        set.get(tab).unwrap().attention.attention().pending == 1
    });
    let permission = {
        let backend = set.get(tab).unwrap().live().unwrap();
        let projection = backend.projection();
        projection.pending_permissions.values().next().unwrap().clone()
    };
    set.get_mut(tab)
        .unwrap()
        .live_mut()
        .unwrap()
        .respond_permission(
            &permission.permission_id,
            agent::PermissionDecision::Deny {
                reason: Some("probe".into()),
            },
        )
        .map_err(|e| e.message)
        .unwrap();
    pump_until(set, dir, "the denied turn to end", |set, _| {
        !set.get(tab).unwrap().turn_running()
    });
    permission
}

/// With no saved rules, Auto offers Bash: a read-only in-project command runs without a card,
/// while a writing command must card -- when Eitri's own policy answers. On a session whose CLI runs
/// its own `auto` the write is deferred to the CLI's classifier with no card, which is checked instead.
#[test]
#[ignore = "real Claude; run under a test-account wrapper, see the module doc"]
fn auto_offers_bash_runs_a_read_only_command_and_cards_a_write() {
    auto_offers_bash_on(BackendKind::Sidecar, "auto-bash");
}

/// The same behavior on legacy, whose Auto CLI gets no `--disallowedTools`.
/// Available only in builds with `--features legacy-backend`.
#[cfg(feature = "legacy-backend")]
#[test]
#[ignore = "real Claude; run under a test-account wrapper, see the module doc"]
fn auto_offers_bash_on_the_legacy_backend_too() {
    auto_offers_bash_on(BackendKind::Legacy, "auto-bash-legacy");
}

/// Whether this tab's session runs the CLI's own `auto` under the gate. Then a call no saved rule
/// allows is deferred to the CLI's classifier: Eitri draws no card and the CLI decides, so the card
/// these probes expect from Eitri's own policy cannot come.
fn cli_runs_auto(set: &TabSet, tab: eitri_core::tabs::TabId) -> bool {
    set.get(tab).unwrap().live().unwrap().projection().cli_runs_auto()
}

/// On a CLI-auto session: sends `prompt`, requires that no card is drawn while the turn runs and
/// that the model really made the Bash call naming `command` (so the deferral was exercised, not
/// skipped). What the CLI's classifier then did with the call is the CLI's own business.
fn the_cli_decides(set: &mut TabSet, dir: &std::path::Path, tab: eitri_core::tabs::TabId, prompt: &str, command: &str) {
    set.get_mut(tab)
        .unwrap()
        .live_mut()
        .unwrap()
        .send_turn(prompt, command, agent::PromptOrigin::Composed)
        .map_err(|e| e.message)
        .unwrap();
    // Until this turn reports its end: right after the send it has not started, so `turn_running`
    // alone would already read false.
    let mut ended_here = false;
    pump_until(set, dir, command, |set, ended| {
        assert_eq!(
            set.get(tab).unwrap().attention.attention().pending,
            0,
            "{command}: a CLI-auto session defers the call to the CLI's classifier; no card"
        );
        ended_here |= ended.contains(&tab);
        ended_here && !set.get(tab).unwrap().turn_running()
    });
    let made = set
        .get(tab)
        .unwrap()
        .live()
        .unwrap()
        .projection()
        .tool_calls
        .iter()
        .any(|call| call.name == "Bash" && call.input["command"].as_str().is_some_and(|c| c.contains(command)));
    assert!(
        made,
        "{command}: the model never made the Bash call, so no deferral was exercised"
    );
    println!("[friction] {command}: the CLI runs auto, so the call went to its classifier with no card");
}

fn auto_offers_bash_on(kind: BackendKind, label: &str) {
    let dir = project(label);
    let (mut set, tab) = live_set_on(kind, &dir);
    set.get_mut(tab)
        .unwrap()
        .live_mut()
        .unwrap()
        .send_turn(
            "Use the Bash tool to run `ls` in the current directory and tell me the file names.",
            "ls",
            agent::PromptOrigin::Composed,
        )
        .map_err(|e| e.message)
        .unwrap();
    pump_until(&mut set, &dir, "the ls turn", |set, _| {
        assert_eq!(
            set.get(tab).unwrap().attention.attention().pending,
            0,
            "a read-only command inside the project needs nobody: no card"
        );
        !set.get(tab).unwrap().turn_running() && transcript(set, tab).to_lowercase().contains("main.rs")
    });
    let ran_ls = set
        .get(tab)
        .unwrap()
        .live()
        .unwrap()
        .projection()
        .tool_calls
        .iter()
        .any(|call| {
            call.name == "Bash"
                && call.input["command"]
                    .as_str()
                    .is_some_and(|c| c.trim_start().starts_with("ls"))
        });
    assert!(ran_ls, "the model had Bash in Auto and ran ls through it");

    if cli_runs_auto(&set, tab) {
        the_cli_decides(
            &mut set,
            &dir,
            tab,
            "Use the Bash tool to run exactly this command, unchanged: touch auto-bash-probe.txt",
            "touch auto-bash-probe.txt",
        );
        shut_down(&mut set);
        return;
    }
    let card = deny_the_next_card(
        &mut set,
        &dir,
        tab,
        "Use the Bash tool to run exactly this command, unchanged: touch auto-bash-probe.txt",
        "touch",
    );
    assert_eq!(card.tool_name, "Bash", "the write was gated as a Bash call: {card:?}");
    assert!(!dir.join("auto-bash-probe.txt").exists(), "denied: nothing was written");
    shut_down(&mut set);
}
