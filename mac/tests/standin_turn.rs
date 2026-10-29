//! Drives the stand-in `claude` through the pinned sidecar without a window, real Claude,
//! network access, or an account. Checks held turns, permission cards and answers,
//! immediate completion, and interruption of a held turn.
//!
//! Ignored by default because a sidecar built for this machine is required. Set its path:
//!
//! ```sh
//! EITRI_SIDECAR_BINARY=<path-to-sidecar> \
//!     cargo test -p eitri-mac --test standin_turn -- --ignored --nocapture
//! ```

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use agent::PermissionDecision;
use eitri_core::agent_backend::{AgentBackend, BackendKind};
use eitri_core::agent_bridge::SessionModeChoice;
use eitri_core::tab_set::{TabBackend, TabSet};
use eitri_core::tabs::TabId;
use serde_json::Value;

fn stand_in() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../mac-spike/app-acceptance/stand-in-claude")
}

/// Shuts every live backend down when the test ends, a failed assertion included.
struct LiveSet(TabSet);

impl Drop for LiveSet {
    fn drop(&mut self) {
        for tab in self.0.take_all() {
            if let Some(retired) = tab.retired {
                retired.into_backend().shutdown();
            }
        }
    }
}

fn events_of(payload: Option<&str>) -> Vec<Value> {
    let Some(payload) = payload else { return Vec::new() };
    let value: Value = serde_json::from_str(payload).unwrap_or(Value::Null);
    if value["kind"] == "events" {
        value["events"].as_array().cloned().unwrap_or_default()
    } else {
        Vec::new()
    }
}

/// Pumps the panel's loop until `done` says so (or `limit` passes), collecting what the panel was sent.
fn pump_until(
    set: &mut TabSet,
    dir: &Path,
    limit: Duration,
    seen: &mut Vec<Value>,
    mut done: impl FnMut(&TabSet, &[Value]) -> bool,
) -> bool {
    let end = Instant::now() + limit;
    while Instant::now() < end {
        let out = set.pump(dir, true);
        if let Some(tripped) = out.tripped.into_iter().next() {
            panic!("the tripwire closed the session: {}", tripped.reason);
        }
        seen.extend(events_of(out.active_payload.as_deref()));
        if done(set, seen) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(33));
    }
    false
}

fn types(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .map(|e| e["type"].as_str().unwrap_or("?").to_owned())
        .collect()
}

#[test]
#[ignore = "needs the sidecar built for this machine (EITRI_SIDECAR_BINARY); see the module doc"]
fn the_stand_in_holds_a_turn_raises_a_card_and_finishes_when_told() {
    let sidecar = std::env::var_os("EITRI_SIDECAR_BINARY").expect("EITRI_SIDECAR_BINARY names the sidecar");
    assert!(Path::new(&sidecar).is_file(), "{sidecar:?} is not a file");
    // Kept after the test (its path is printed): the stand-in's transcript is the evidence of what it was sent.
    let root = tempfile::Builder::new()
        .prefix("eitri-standin-")
        .tempdir_in("/tmp")
        .unwrap()
        .keep();
    println!("[stand-in] scratch {}", root.display());
    let (state, control, project) = (root.join("s"), root.join("c"), root.join("p"));
    for dir in [&state, &control, &project] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::env::set_var("XDG_STATE_HOME", &state);
    std::env::set_var("VERDANDI_CLAUDE_CLI_PATH", stand_in());
    std::env::set_var("EITRI_STANDIN_DIR", &control);
    std::env::remove_var("VERDANDI_CLAUDE_ACCOUNT");
    std::env::remove_var("EITRI_AGENT_BACKEND");
    let dir = project.canonicalize().unwrap();

    let mut live = LiveSet(TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto));
    let set = &mut live.0;
    let tab: TabId = set.active();
    let backend = AgentBackend::start(
        BackendKind::Sidecar,
        &dir,
        None,
        agent::setting_sources::ProjectTrust::Untrusted,
    )
    .map_err(|e| e.message)
    .expect("a sidecar session starts with the stand-in");
    set.get_mut(tab).unwrap().set_backend(TabBackend::Live(backend));

    // ---- 1: a held turn with a card.
    set.get_mut(tab)
        .unwrap()
        .live_mut()
        .unwrap()
        .send_turn("[card] first", "[card] first", agent::PromptOrigin::Composed)
        .map_err(|e| e.message)
        .unwrap();
    let mut seen = Vec::new();
    let carded = pump_until(set, &dir, Duration::from_secs(30), &mut seen, |_, seen| {
        seen.iter().any(|e| e["type"] == "permission_requested")
    });
    assert!(carded, "a card reached the panel: {:?}", types(&seen));
    let card = seen
        .iter()
        .find(|e| e["type"] == "permission_requested")
        .unwrap()
        .clone();
    println!("[stand-in] card: {card}");
    let id = card["permission_id"].as_str().unwrap().to_owned();
    set.get_mut(tab)
        .unwrap()
        .live_mut()
        .unwrap()
        .respond_permission(
            &id,
            PermissionDecision::Deny {
                reason: Some("the stand-in test".into()),
            },
        )
        .map_err(|e| e.message)
        .unwrap();
    let reported = pump_until(set, &dir, Duration::from_secs(15), &mut seen, |_, seen| {
        seen.iter().any(|e| e["type"] == "tool_call_completed")
    });
    assert!(
        reported,
        "the denied call came back as a tool result: {:?}",
        types(&seen)
    );
    // Held: still running after two more seconds.
    pump_until(set, &dir, Duration::from_secs(2), &mut seen, |_, _| false);
    assert!(set.get(tab).unwrap().turn_running(), "the turn is held open");
    let mode = set
        .get(tab)
        .unwrap()
        .live()
        .unwrap()
        .projection()
        .cli_mode_reported
        .clone();
    assert_eq!(mode.as_deref(), Some("default"), "the stand-in reports default");
    std::fs::write(control.join("finish"), "").unwrap();
    let ended = pump_until(set, &dir, Duration::from_secs(15), &mut seen, |set, _| {
        !set.get(tab).unwrap().turn_running()
    });
    assert!(ended, "the turn ended once told to: {:?}", types(&seen));
    let text = seen
        .iter()
        .any(|e| e.to_string().contains("stand-in: working on turn 1"));
    assert!(text, "its text reached the panel: {:?}", types(&seen));
    println!("[stand-in] turn 1 events: {:?}", types(&seen));

    // ---- 2: a turn that finishes at once.
    // The panel's loop folds the end of the last turn before the next is sent, as a user's next message would be.
    pump_until(set, &dir, Duration::from_millis(500), &mut Vec::new(), |_, _| false);
    let mut seen = Vec::new();
    set.get_mut(tab)
        .unwrap()
        .live_mut()
        .unwrap()
        .send_turn("[now] second", "[now] second", agent::PromptOrigin::Composed)
        .unwrap();
    let ended = pump_until(set, &dir, Duration::from_secs(15), &mut seen, |set, seen| {
        !seen.is_empty() && !set.get(tab).unwrap().turn_running()
    });
    assert!(ended, "a [now] turn finishes by itself: {:?}", types(&seen));

    // ---- 3: an interrupt ends a held turn.
    // The panel's loop folds the end of the last turn before the next is sent, as a user's next message would be.
    pump_until(set, &dir, Duration::from_millis(500), &mut Vec::new(), |_, _| false);
    let mut seen = Vec::new();
    set.get_mut(tab)
        .unwrap()
        .live_mut()
        .unwrap()
        .send_turn("third", "third", agent::PromptOrigin::Composed)
        .unwrap();
    let running = pump_until(set, &dir, Duration::from_secs(15), &mut seen, |set, _| {
        set.get(tab).unwrap().turn_running()
    });
    assert!(running, "the third turn runs");
    pump_until(set, &dir, Duration::from_secs(1), &mut seen, |_, _| false);
    set.get_mut(tab).unwrap().live_mut().unwrap().interrupt().unwrap();
    let ended = pump_until(set, &dir, Duration::from_secs(15), &mut seen, |set, _| {
        !set.get(tab).unwrap().turn_running()
    });
    assert!(ended, "an interrupt ends a held turn: {:?}", types(&seen));
    println!("[stand-in] turn 3 events: {:?}", types(&seen));

    // What the stand-in was given: the oracle for what a turn carries.
    let transcript = std::fs::read_to_string(control.join("transcript.jsonl")).unwrap();
    let users = transcript.lines().filter(|l| l.contains("\"type\": \"user\"")).count();
    println!(
        "[stand-in] transcript: {} lines, {users} user lines",
        transcript.lines().count()
    );
    assert!(transcript.contains("[card] first") && transcript.contains("third"));
}
