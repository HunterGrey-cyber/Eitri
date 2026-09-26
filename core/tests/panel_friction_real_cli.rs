//! Phase 3 against a real sidecar and a real `claude` (keymap/tabs spec §4.5). `#[ignore]`d: it
//! spawns real Claude sessions and bills the account they run as.
//!
//! **Run only on the TEST profile, through the resolved binary, with a scratch state home:**
//!
//! ```sh
//! claude --version    # record the build
//! XDG_STATE_HOME=/tmp/nv-friction-real-state \
//!     cargo test -p neovibe-core --test panel_friction_real_cli -- --ignored --nocapture --test-threads=1
//! ```

use neovibe_core::agent_backend::{AgentBackend, BackendKind};
use neovibe_core::agent_bridge::SessionModeChoice;
use neovibe_core::tab_set::{SendNow, TabBackend, TabSet};
use std::time::{Duration, Instant};

fn project(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("nv-friction-real-{label}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("main.rs"), "fn marmalade() {}\n").unwrap();
    dir.canonicalize().unwrap()
}

fn live_set(dir: &std::path::Path) -> (TabSet, neovibe_core::tabs::TabId) {
    let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    let tab = set.active();
    let backend = AgentBackend::start(BackendKind::Sidecar, dir, agent::PermissionMode::Auto, None)
        .map_err(|e| e.message)
        .expect("a sidecar session starts; is this running under a test-account wrapper?");
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    (set, tab)
}

fn transcript(set: &TabSet, tab: neovibe_core::tabs::TabId) -> String {
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
    mut done: impl FnMut(&mut TabSet, Vec<neovibe_core::tabs::TabId>) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(240);
    loop {
        let ended = set.pump(dir, true).turn_ended;
        if done(set, ended) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(33));
    }
}

fn shut_down(set: &mut TabSet) {
    for mut tab in set.take_all() {
        if let TabBackend::Live(backend) = &mut tab.backend {
            backend.shutdown();
        }
    }
}

/// D4 + D5 A: a message queued during a turn goes out after `Ctrl+Enter` interrupts it.
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

/// D7: a project rule answers a matching Bash call with no card; a writing option still cards.
///
/// Spec §4.5 names "a `git log *` rule answered with no card". `git log` is already on the
/// classifier's read-only `git` list, so that probe would pass with no rule at all and prove
/// nothing; `uname` is refused for exactly the replaceable reason ("not on the CLI's read-only
/// list"), so a rule is what answers it. The `git log --output=` half is the spec's own.
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
    // Not vacuous (the phase-3 GUI pass, 2026-09-25): the first run of this test passed its first
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
    assert!(
        ran_bash,
        "no Bash call reached the gate: Auto disallows Bash (agent::disallowed_tools_for), so a D7 rule has nothing to answer"
    );

    set.get_mut(tab)
        .unwrap()
        .live_mut()
        .unwrap()
        .send_turn(
            "Use the Bash tool to run exactly this command, unchanged: git log --output=friction-probe.txt -1",
            "git log output",
        )
        .map_err(|e| e.message)
        .unwrap();
    pump_until(&mut set, &dir, "the git log card", |set, _| {
        set.get(tab).unwrap().attention.attention().pending == 1
    });
    let permission_id = {
        let backend = set.get(tab).unwrap().live().unwrap();
        let projection = backend.projection();
        projection.pending_permissions.keys().next().unwrap().clone()
    };
    set.get_mut(tab)
        .unwrap()
        .live_mut()
        .unwrap()
        .respond_permission(
            &permission_id,
            agent::PermissionDecision::Deny {
                reason: Some("probe".into()),
            },
        )
        .map_err(|e| e.message)
        .unwrap();
    assert!(!dir.join("friction-probe.txt").exists(), "denied: nothing was written");
    shut_down(&mut set);
}
