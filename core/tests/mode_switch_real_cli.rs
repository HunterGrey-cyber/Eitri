//! Wave-5 Task 7: the mid-session mode switch (Task 2) against a real sidecar and a real `claude`.
//! `#[ignore]`d: it spawns real Claude sessions and bills the account they run as.
//!
//! **Run only on the TEST profile, through the resolved binary, with a scratch state home:**
//!
//! ```sh
//! claude --version
//! XDG_STATE_HOME=$HOME/.cache/nv-wave5-t7/state \
//!     cargo test -p neovibe-core --test mode_switch_real_cli -- --ignored --nocapture --test-threads=1
//! ```

use neovibe_core::agent_backend::{AgentBackend, BackendKind};
use neovibe_core::agent_bridge::SessionModeChoice;
use neovibe_core::tab_set::{TabBackend, TabSet};
use neovibe_core::tabs::TabId;
use std::time::{Duration, Instant};

/// `$HOME/.cache`, never `temp_dir()` (`/tmp` is a small shared tmpfs on this harness).
fn scratch_root() -> std::path::PathBuf {
    let home = std::env::var("HOME").expect("HOME must be set");
    std::path::PathBuf::from(home).join(".cache").join("nv-wave5-t7")
}

fn project(label: &str) -> std::path::PathBuf {
    let dir = scratch_root().join(format!("{label}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("main.rs"), "fn marmalade() {}\n").unwrap();
    dir.canonicalize().unwrap()
}

/// Modeled on `panel_friction_real_cli.rs`'s `live_set_on`, with the session's actual permission
/// mode as a parameter -- the friction test only ever started `Auto`, and W3 (a session started in
/// bypass) needs a real Bypass `CreateSession`. `TabSet::new`'s `default_mode` becomes the one tab's
/// own `mode` too, so a bypass start also leaves `tab.mode` where `switch_mode`'s `cycled()` expects
/// it (Bypass -> Auto), not just the backend.
///
/// Returns a `LiveSet`, not a bare `TabSet`: these tests spawn a real Verdandi sidecar and a real
/// billed `claude` child, and an assertion failure or a `pump_until`/`pump_backend_events` timeout
/// partway through unwinds out of the test function. `LiveSet`'s `Drop` calls `shut_down` on
/// whatever tabs are still there, on that failure path as well as the ordinary end of the test, so
/// no run leaves an orphaned subprocess behind.
fn live_set_on(kind: BackendKind, dir: &std::path::Path, mode: agent::PermissionMode) -> (LiveSet, TabId) {
    let mode_choice = match mode {
        agent::PermissionMode::Auto => SessionModeChoice::Auto,
        agent::PermissionMode::Bypass => SessionModeChoice::Bypass,
    };
    let mut set = TabSet::new(kind, mode_choice);
    let tab = set.active();
    let backend = AgentBackend::start(kind, dir, mode, None)
        .map_err(|e| e.message)
        .expect("a sidecar session starts; is this running under a test-account wrapper?");
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    (LiveSet(set), tab)
}

/// Owns a test's `TabSet` and guarantees `shut_down` runs even when the test panics before reaching
/// its own explicit `shut_down` call -- the Write-card timeout, a file-content assert, the
/// no-`PermissionRequested` assert, or `pump_until`/`pump_backend_events` themselves timing out
/// (they `panic!()`) all unwind through here. `shut_down` is idempotent (`TabSet::take_all` empties
/// the set with `std::mem::take` on its first call), so `Drop` running after an explicit call at
/// the end of a passing test is a harmless no-op, not a double shutdown.
///
/// `Deref`/`DerefMut` to `TabSet` so every existing call site (`set.get(tab)`, `set.pump(..)`,
/// `set.switch_mode(tab)`, passing `&mut set` to `pump_until`) keeps working unchanged.
struct LiveSet(TabSet);

impl std::ops::Deref for LiveSet {
    type Target = TabSet;
    fn deref(&self) -> &TabSet {
        &self.0
    }
}

impl std::ops::DerefMut for LiveSet {
    fn deref_mut(&mut self) -> &mut TabSet {
        &mut self.0
    }
}

impl Drop for LiveSet {
    fn drop(&mut self) {
        shut_down(&mut self.0);
    }
}

fn pump_until(
    set: &mut TabSet,
    dir: &std::path::Path,
    what: &str,
    mut done: impl FnMut(&mut TabSet, Vec<TabId>) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(240);
    loop {
        let ended = set.pump(dir, true).turn_ended;
        if done(set, ended) {
            return;
        }
        if Instant::now() >= deadline {
            let tab = set.active();
            let state = set.get(tab).and_then(|t| t.live()).map(|b| {
                let p = b.projection();
                format!(
                    "running={} pending={} tools={:?}",
                    set.get(tab).unwrap().turn_running(),
                    p.pending_permissions.len(),
                    p.tool_calls
                        .iter()
                        .map(|c| (c.name.clone(), c.input.clone()))
                        .collect::<Vec<_>>(),
                )
            });
            panic!("timed out: {what}; {state:?}");
        }
        std::thread::sleep(Duration::from_millis(33));
    }
}

/// Drives one live backend directly, bypassing `TabSet::pump` -- which only serializes the ACTIVE
/// tab's delta into an opaque JSON payload for the panel -- so every delivered
/// `AgentDomainEvent` can be inspected. Needed to assert the negative (no `PermissionRequested`
/// ever arrives) rather than only a side effect of one.
///
/// A `Resync` here would mean the queue overflowed (`UI_EVENT_QUEUE_CAPACITY` = 256) and events
/// were dropped: this test cannot then tell "no card was raised" from "a card was raised and lost",
/// so it fails loudly instead of passing on missing evidence.
fn pump_backend_events(backend: &mut AgentBackend, dir: &std::path::Path, what: &str) -> Vec<agent::AgentDomainEvent> {
    let deadline = Instant::now() + Duration::from_secs(240);
    let mut collected = Vec::new();
    loop {
        match backend.take_ui_delivery(dir) {
            agent::UiDelivery::Events(events) => {
                let ended = events
                    .iter()
                    .any(|e| matches!(e, agent::AgentDomainEvent::TurnCompleted { .. }));
                collected.extend(events);
                if ended {
                    return collected;
                }
            }
            agent::UiDelivery::Resync => {
                panic!("resynced mid-poll ({what}): events may have been dropped, collected so far: {collected:?}")
            }
            agent::UiDelivery::Nothing => {}
        }
        if Instant::now() >= deadline {
            panic!("timed out: {what}; events so far: {collected:?}");
        }
        std::thread::sleep(Duration::from_millis(33));
    }
}

/// The `tool_use_id`s of every `ToolCallStarted` for `tool_name` in `events`.
fn tool_use_ids_for<'a>(events: &'a [agent::AgentDomainEvent], tool_name: &str) -> Vec<&'a str> {
    events
        .iter()
        .filter_map(|e| match e {
            agent::AgentDomainEvent::ToolCallStarted { tool_use_id, name, .. } if name == tool_name => {
                Some(tool_use_id.as_str())
            }
            _ => None,
        })
        .collect()
}

/// Whether any `ToolCallCompleted` among `events` whose `tool_use_id` is in `ids` completed
/// successfully (`is_error == false`). `ToolCallCompleted` carries no tool name of its own -- only
/// `ToolCallStarted` does -- so the two must be correlated by id.
fn any_completed_ok(events: &[agent::AgentDomainEvent], ids: &[&str]) -> bool {
    events.iter().any(|e| match e {
        agent::AgentDomainEvent::ToolCallCompleted {
            tool_use_id, is_error, ..
        } => ids.contains(&tool_use_id.as_str()) && !is_error,
        _ => false,
    })
}

fn shut_down(set: &mut TabSet) {
    for mut tab in set.take_all() {
        if let TabBackend::Live(backend) = &mut tab.backend {
            backend.shutdown();
        }
    }
}

/// W1-W4: a live Auto session switches to Bypass (the waiting card is auto-approved, W4), a Read
/// outside the project then runs completely unasked (Bypass installs no gate at all), and switching
/// back to Auto cards a fresh Write again.
#[test]
#[ignore = "real Claude; run under a test-account wrapper, see the module doc"]
fn a_live_session_switches_both_ways_and_bypass_answers_the_waiting_card() {
    let dir = project("switch");
    let (mut set, tab) = live_set_on(BackendKind::Sidecar, &dir, agent::PermissionMode::Auto);
    assert!(
        set.get(tab).unwrap().live().unwrap().can_switch_mode(),
        "the sidecar must advertise set_permission_mode (Verdandi 133dc03); if this fails the \
         artifact under test is not built from that revision -- advertised capabilities: {:?}",
        set.get(tab)
            .unwrap()
            .live()
            .unwrap()
            .provider_info()
            .map(|p| &p.advertised_capabilities)
    );

    // (1) send a Write; wait for exactly one card.
    set.get_mut(tab)
        .unwrap()
        .live_mut()
        .unwrap()
        .send_turn(
            "Use the Write tool to create out.txt containing hi. Do nothing else.",
            "write-1",
        )
        .map_err(|e| e.message)
        .unwrap();
    pump_until(&mut set, &dir, "the first Write card", |set, _| {
        set.get(tab).unwrap().attention.attention().pending == 1
    });

    // (2) switch to bypass; the waiting card is auto-approved (W4) and the turn finishes on its own.
    assert_eq!(set.switch_mode(tab), Ok(SessionModeChoice::Bypass));
    pump_until(&mut set, &dir, "the switch-to-bypass turn to end", |set, _| {
        !set.get(tab).unwrap().turn_running()
    });
    assert_eq!(
        std::fs::read_to_string(dir.join("out.txt")).unwrap().trim(),
        "hi",
        "the card pending at the moment of the switch was auto-approved (W4)"
    );
    assert_eq!(
        set.get(tab).unwrap().attention.attention().pending,
        0,
        "no card should remain pending once bypass answered it"
    );

    // (3) a Read outside the project: no PermissionRequested at all (bypass installs no gate), and
    // a ToolCallCompleted for it with is_error == false -- in Auto that same path is a card.
    let outside = scratch_root().join("outside-switch.txt");
    std::fs::write(&outside, "outside\n").unwrap();
    {
        let backend = set.get_mut(tab).unwrap().live_mut().unwrap();
        backend
            .send_turn(
                &format!("Use the Read tool on {} and tell me what it says.", outside.display()),
                "read-outside",
            )
            .map_err(|e| e.message)
            .unwrap();
        let events = pump_backend_events(backend, &dir, "the outside-the-project Read, in bypass");
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, agent::AgentDomainEvent::PermissionRequested { .. })),
            "bypass installs no PreToolUse gate at all: no card should ever be raised: {events:?}"
        );
        let read_ids = tool_use_ids_for(&events, "Read");
        assert!(!read_ids.is_empty(), "the model should have called Read: {events:?}");
        assert!(
            any_completed_ok(&events, &read_ids),
            "the outside-the-project Read must complete with is_error == false in bypass: {events:?}"
        );
    }

    // (4) switch back to auto; a fresh Write is carded again; deny it.
    assert_eq!(set.switch_mode(tab), Ok(SessionModeChoice::Auto));
    set.get_mut(tab)
        .unwrap()
        .live_mut()
        .unwrap()
        .send_turn(
            "Use the Write tool to create out2.txt containing hi. Do nothing else.",
            "write-2",
        )
        .map_err(|e| e.message)
        .unwrap();
    pump_until(&mut set, &dir, "the second Write card", |set, _| {
        set.get(tab).unwrap().attention.attention().pending == 1
    });
    let permission = {
        let backend = set.get(tab).unwrap().live().unwrap();
        backend
            .projection()
            .pending_permissions
            .values()
            .next()
            .unwrap()
            .clone()
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
    pump_until(&mut set, &dir, "the denied second-Write turn to end", |set, _| {
        !set.get(tab).unwrap().turn_running()
    });
    assert!(
        !dir.join("out2.txt").exists(),
        "denied after the switch back to auto: nothing should have been written"
    );

    shut_down(&mut set);
}

/// W3: a tab started in bypass on a switch-capable sidecar is created gated and switched to bypass
/// right after `CreateSession` (`initial_policy`/`finish_start`), never a session created directly
/// under BYPASS -- which the sidecar contract says can never return to `INTERACTIVE`. So a session
/// that started this way must still run an outside-the-project Read unasked, AND still switch back
/// to auto; a session genuinely created under BYPASS would be refused on the second half.
#[test]
#[ignore = "real Claude; run under a test-account wrapper, see the module doc"]
fn a_session_started_in_bypass_runs_unasked_and_can_return_to_auto() {
    let dir = project("bypass-start");
    let (mut set, tab) = live_set_on(BackendKind::Sidecar, &dir, agent::PermissionMode::Bypass);

    let outside = scratch_root().join("outside-bypass-start.txt");
    std::fs::write(&outside, "outside\n").unwrap();
    {
        let backend = set.get_mut(tab).unwrap().live_mut().unwrap();
        backend
            .send_turn(
                &format!("Use the Read tool on {} and tell me what it says.", outside.display()),
                "read-outside",
            )
            .map_err(|e| e.message)
            .unwrap();
        let events = pump_backend_events(backend, &dir, "a bypass-started session's outside-the-project Read");
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, agent::AgentDomainEvent::PermissionRequested { .. })),
            "a session started in bypass installs no gate at all: {events:?}"
        );
        let read_ids = tool_use_ids_for(&events, "Read");
        assert!(!read_ids.is_empty(), "the model should have called Read: {events:?}");
        assert!(
            any_completed_ok(&events, &read_ids),
            "the Read must complete with is_error == false: {events:?}"
        );
    }

    assert_eq!(
        set.switch_mode(tab),
        Ok(SessionModeChoice::Auto),
        "a session started in bypass was created gated and switched (W3); it must still be able to \
         switch back to auto. A refusal here means the session was created directly under BYPASS \
         instead (check agent::providers::claude_sidecar::initial_policy)"
    );

    shut_down(&mut set);
}
