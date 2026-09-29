//! Real-CLI probes of the gated bypass (v1 mode plan Task 5, spec §2.4/§2.5, O3). `#[ignore]`d: it
//! spawns real Claude sessions and bills the account they run as.
//!
//! **Run only on the TEST profile, through the resolved binary, with a scratch state home:**
//!
//! ```sh
//! claude --version    # record the build; nothing prints it for you
//! XDG_STATE_HOME=$HOME/.cache/nv-v1mode-t5/state \
//!     cargo test -p neovibe-core --test v1_mode_real_cli -- --ignored --nocapture --test-threads=1
//! ```
//!
//! The four legacy tests (2, 3 on legacy, 5 on legacy twice) exist only in a build with the legacy
//! backend: add `--features legacy-backend` to run them (spec 2026-09-27-v1-dist-design.md §10, D16).
//!
//! The test-account wrapper sets `VERDANDI_CLAUDE_CLI_PATH` to the `claude-wrapper` launcher, which is what
//! decides the sidecar's CLI (CLAUDE.md, the environment table); `PATH` alone does not.
//!
//! **What these prove, and what they only record.** R07/S2 say bypass is neovibe's own `allow`
//! under a CLI that always runs gated -- never `bypassPermissions` on the wire. Tests 1/1b/2/6 assert
//! that: no `PermissionRequested` a bypass tab answers itself is ever delivered to the panel, the
//! resolution still arrives (sidecar) or is silently dropped with the request (legacy, D4's own
//! documented gap), and the effect lands on disk. Tests 3, 4 and 5 are the probes spec O3 asks for --
//! whether the real CLI's own `default` mode plus a hook that always allows behaves exactly like
//! `bypassPermissions` would. **A refusal there is not this test suite's bug to fix**: per O3, it must
//! be recorded verbatim (the tool result text, the CLI build) and reported in the task's `concerns`,
//! never loosened or skipped around.
//!
//! **A run that did not exercise what a test is about fails as INCONCLUSIVE** (v1-mode fix round 1,
//! after Codex's findings 2-4): test 1b needs exactly five Reads each paired with its own resolution
//! by id; test 3 needs an `Agent`/`Task` call and, for a `Write` card, proof from the CLI's own
//! transcript that a subagent made it; test 5 counts a trip after the card as a trip. Read the raw
//! lines each prints before recording any figure or branch.

use agent::{AgentDomainEvent, PermissionDecision, PermissionMode, PermissionOutcome, PrefixRules};
use neovibe_core::agent_backend::{AgentBackend, BackendKind};
use neovibe_core::agent_bridge::SessionModeChoice;
use neovibe_core::tab_set::{ConfirmOutcome, ModeCycle, TabBackend, TabSet};
use neovibe_core::tabs::TabId;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// `$HOME/.cache/nv-v1mode-t5/<pid>/` -- never `/tmp` (Global Constraints), and namespaced by this
/// process's own pid so two runs (or two lanes) never collide on the same scratch tree.
fn scratch_root() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME must be set");
    PathBuf::from(home)
        .join(".cache")
        .join("nv-v1mode-t5")
        .join(std::process::id().to_string())
}

/// A project directory with one file already in it, canonicalized so every consumer downstream
/// (the classifier's boundary check included) sees the same spelling this test does.
fn project(label: &str) -> PathBuf {
    let dir = scratch_root().join(label).join("project");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("main.rs"), "fn marmalade() {}\n").unwrap();
    dir.canonicalize().unwrap()
}

/// A scratch directory OUTSIDE any project, for the effects probes that write/read/delete things a
/// real `bypassPermissions` session would reach beyond its own working directory.
fn scratch(label: &str, name: &str) -> PathBuf {
    let dir = scratch_root().join(label).join(name);
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

/// Shuts down every live backend still held by `set`. Idempotent: `take_all` empties the tab list,
/// so calling this twice (once explicitly, once from `LiveSet::drop`) does nothing the second time.
fn shut_down(set: &mut TabSet) {
    for mut tab in set.take_all() {
        if let TabBackend::Live(backend) = &mut tab.backend {
            backend.shutdown();
        }
    }
}

/// Owns a `TabSet` and shuts down everything it holds even when an assertion panics mid-test --
/// Rust still runs `Drop` while unwinding, which a `shut_down` call written only at a test's last
/// line does not cover, and a leaked real `claude`/sidecar process from a failed assertion is
/// exactly the accident the brief's "a guard struct shuts every tab down on failure too" is for.
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

/// The same guard for a test that drives one `AgentBackend` directly, with no `TabSet` at all
/// (`host_answer_latency_in_bypass`).
struct LiveBackend(AgentBackend);

impl std::ops::Deref for LiveBackend {
    type Target = AgentBackend;
    fn deref(&self) -> &AgentBackend {
        &self.0
    }
}

impl std::ops::DerefMut for LiveBackend {
    fn deref_mut(&mut self) -> &mut AgentBackend {
        &mut self.0
    }
}

impl Drop for LiveBackend {
    fn drop(&mut self) {
        self.0.shutdown();
    }
}

/// A tab confirmed into bypass BEFORE it has a backend (D2/D13, ruling 5: an empty tab's own confirm
/// carries the window default with it). Mirrors what a real launch does when the user answers `y` to
/// the dashboard's own bypass offer before the first turn.
fn bypass_tab(kind: BackendKind, dir: &std::path::Path) -> (LiveSet, TabId) {
    let mut set = TabSet::new(kind, SessionModeChoice::Auto);
    let tab = set.active();
    let plan = match set
        .cycle_mode(tab)
        .expect("an empty tab can always start a bypass entry")
    {
        ModeCycle::Confirm(plan) => plan,
        ModeCycle::Changed(mode) => panic!("a fresh auto tab must ask first (D2), got Changed({mode:?})"),
    };
    assert_eq!(plan.approve.len(), 0, "an empty tab has no cards to approve yet");
    match set
        .confirm_bypass(plan.scope, plan.nonce)
        .expect("the just-built plan is still current")
    {
        ConfirmOutcome::Entered { approved, .. } => assert_eq!(approved, 0),
        other => panic!("expected Entered{{approved: 0}}, got {other:?}"),
    }
    assert_eq!(
        set.get(tab).unwrap().mode(),
        SessionModeChoice::Bypass,
        "D2/D13: confirmed before the backend even exists"
    );
    let backend = AgentBackend::start(kind, dir, None)
        .map_err(|e| e.message)
        .expect("a session starts; is this running under a test-account wrapper?");
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    (LiveSet(set), tab)
}

/// A tab in the ordinary `auto` default, with a live backend already attached.
fn auto_tab(kind: BackendKind, dir: &std::path::Path) -> (LiveSet, TabId) {
    let mut set = TabSet::new(kind, SessionModeChoice::Auto);
    let tab = set.active();
    let backend = AgentBackend::start(kind, dir, None)
        .map_err(|e| e.message)
        .expect("a session starts; is this running under a test-account wrapper?");
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    (LiveSet(set), tab)
}

fn transcript(set: &TabSet, tab: TabId) -> String {
    let projection = set.get(tab).unwrap().live().unwrap().projection();
    projection
        .transcript
        .iter()
        .map(|m| m.text.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Pulls the raw `AgentDomainEvent`s (still tagged JSON, `"type": "..."`) a pump's `active_payload`
/// carried, or nothing for a `"snapshot"` (Resync) payload or an inactive tick.
fn events_from_payload(payload: Option<&str>) -> Vec<serde_json::Value> {
    let Some(payload) = payload else { return Vec::new() };
    let value: serde_json::Value = serde_json::from_str(payload).unwrap();
    if value["kind"] == "events" {
        value["events"].as_array().cloned().unwrap_or_default()
    } else {
        Vec::new()
    }
}

/// Sends `prompt` and pumps the real 33ms loop until the turn has both started and ended, handing
/// every newly delivered event to `on_events` along the way. `turn_running()` reads the backend's
/// own live projection directly (not a locally cached flag), but the sidecar's `active_turn_id` only
/// becomes `Some` once the provider's real `TurnStarted` folds -- asynchronously, on the ingestion
/// thread -- so "wait for it to become true" is a real phase here, not a formality: reading `false`
/// straight after `send_turn` must never be mistaken for "the turn already ended".
///
/// Also watches for the D12 tripwire: a tab this pump fails mid-turn is shut down and reported
/// immediately rather than spun on until the deadline pretends the turn "never started/ended".
fn drive_turn(
    set: &mut TabSet,
    dir: &std::path::Path,
    tab: TabId,
    prompt: &str,
    label: &str,
    mut on_events: impl FnMut(&mut TabSet, &[serde_json::Value]),
) {
    set.get_mut(tab)
        .unwrap()
        .live_mut()
        .unwrap()
        .send_turn(prompt, label)
        .map_err(|e| e.message)
        .unwrap();

    let started_deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let out = set.pump(dir, true);
        let events = events_from_payload(out.active_payload.as_deref());
        on_events(set, &events);
        if let Some(mut t) = out.tripped.into_iter().next() {
            let reason = t.reason.clone();
            t.backend.shutdown();
            panic!("{label}: the D12 tripwire fired before the turn even started: {reason}");
        }
        if set.get(tab).unwrap().turn_running() {
            break;
        }
        assert!(Instant::now() < started_deadline, "{label}: the turn never started");
        std::thread::sleep(Duration::from_millis(33));
    }

    let ended_deadline = Instant::now() + Duration::from_secs(240);
    loop {
        let out = set.pump(dir, true);
        let events = events_from_payload(out.active_payload.as_deref());
        on_events(set, &events);
        if let Some(mut t) = out.tripped.into_iter().next() {
            let reason = t.reason.clone();
            t.backend.shutdown();
            panic!("{label}: the D12 tripwire fired mid-turn: {reason}");
        }
        if !set.get(tab).unwrap().turn_running() {
            break;
        }
        assert!(Instant::now() < ended_deadline, "{label}: the turn never ended");
        std::thread::sleep(Duration::from_millis(33));
    }
}

/// `drive_turn`, collecting every event it saw into one `Vec` for the caller to assert on.
fn effects_turn(
    set: &mut TabSet,
    dir: &std::path::Path,
    tab: TabId,
    prompt: &str,
    label: &str,
) -> Vec<serde_json::Value> {
    let mut events = Vec::new();
    drive_turn(set, dir, tab, prompt, label, |_set, new| {
        events.extend(new.iter().cloned())
    });
    events
}

fn assert_no_permission_requested(events: &[serde_json::Value], label: &str) {
    assert!(
        !events.iter().any(|e| e["type"] == "permission_requested"),
        "{label}: bypass must answer every request itself; the panel must never see one (R07/S2): {events:#?}"
    );
}

/// The sidecar's own resolution still arrives on a later pump (`answer_what_needs_no_human`'s own
/// doc), for a `permission_id` the panel never saw a request for. Legacy's synchronous resolution is
/// dropped with the request instead (D4's documented gap) -- callers on legacy must not call this.
fn assert_permission_resolved_allowed(events: &[serde_json::Value], label: &str) {
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "permission_resolved" && e["outcome"] == "allowed"),
        "{label}: the sidecar's own resolution must still arrive even though no card was ever drawn: {events:#?}"
    );
}

/// At least one `ToolCallStarted` named `tool_name` exists, and every `ToolCallCompleted` for the
/// same `tool_use_id` reports `is_error: false`.
fn assert_tool_ran_without_error(events: &[serde_json::Value], tool_name: &str, label: &str) {
    let started_ids: Vec<String> = events
        .iter()
        .filter(|e| e["type"] == "tool_call_started" && e["name"] == tool_name)
        .filter_map(|e| e["tool_use_id"].as_str().map(str::to_string))
        .collect();
    assert!(
        !started_ids.is_empty(),
        "{label}: expected a {tool_name} call to start: {events:#?}"
    );
    for id in &started_ids {
        let completed = events
            .iter()
            .find(|e| e["type"] == "tool_call_completed" && &e["tool_use_id"] == id)
            .unwrap_or_else(|| panic!("{label}: {tool_name} call {id} never completed: {events:#?}"));
        assert_eq!(
            completed["is_error"], false,
            "{label}: {tool_name} ran without error: {completed:#?}"
        );
    }
}

/// Prints every `tool_name` call the turn made -- its input, and its result text with `is_error` --
/// BEFORE the caller asserts on the effect, so a run that fails an effect assertion (an `rm -rf` that
/// left the directory behind) has already said what the tool was really given and what it answered
/// (v1-mode fix round 1: the legacy run's `rm` did not delete the directory, and nothing recorded why).
fn print_tool_io(events: &[serde_json::Value], tool_name: &str, label: &str) {
    let calls: Vec<&serde_json::Value> = events
        .iter()
        .filter(|e| e["type"] == "tool_call_started" && e["name"] == tool_name)
        .collect();
    if calls.is_empty() {
        println!("[v1-mode] {label}: no {tool_name} call started");
    }
    for call in calls {
        let id = call["tool_use_id"].as_str().unwrap_or("");
        println!("[v1-mode] {label}: {tool_name} {id} input={}", call["input"]);
        match events
            .iter()
            .find(|e| e["type"] == "tool_call_completed" && e["tool_use_id"].as_str() == Some(id))
        {
            Some(done) => println!(
                "[v1-mode] {label}: {tool_name} {id} is_error={} result={}",
                done["is_error"], done["content"]
            ),
            None => println!("[v1-mode] {label}: {tool_name} {id} never completed"),
        }
    }
}

/// Test 1: `Write` outside the project, `Bash rm -rf` of a scratch dir outside the project, `Read` of a
/// file outside the project -- three calls a real `bypassPermissions` session would run without asking,
/// on a session that never once ran with that flag (R07/S2). Every call: no card drawn, the tool
/// completes without error, the effect is really on disk, and the sidecar's own `PermissionResolved`
/// still shows up (evidence the host really answered it, not that nothing was asked at all).
#[test]
#[ignore = "real Claude; run under a test-account wrapper, see the module doc"]
fn sidecar_bypass_answers_every_call_and_the_cli_runs_them() {
    let label = "bypass-effects-sidecar";
    let dir = project(label);
    let (mut set, tab) = bypass_tab(BackendKind::Sidecar, &dir);

    let outside = scratch(label, "outside");
    let write_target = outside.join("w.txt");
    let victim = scratch(label, "victim");
    std::fs::write(victim.join("keep.txt"), "erase me").unwrap();
    // A plainly named file outside the project, not an `id_rsa`-shaped one: the auto policy cards any
    // Read outside the root either way, and the 2026-09-27 real run showed the model refusing to read a
    // file named `id_rsa` at all ("credential-exfiltration pattern"), which measured nothing about bypass.
    let read_dir = scratch(label, "outside-read");
    let read_target = read_dir.join("outside-notes.txt");
    std::fs::write(&read_target, "OUTSIDE-READ-OK\n").unwrap();

    let events = effects_turn(
        &mut set,
        &dir,
        tab,
        &format!(
            "Use the Write tool to create the file at the exact absolute path {} with the exact \
             content OUTSIDE-OK and nothing else.",
            write_target.display()
        ),
        "write-outside",
    );
    print_tool_io(&events, "Write", "write-outside");
    assert_no_permission_requested(&events, "write-outside");
    assert_permission_resolved_allowed(&events, "write-outside");
    assert_tool_ran_without_error(&events, "Write", "write-outside");
    assert!(write_target.exists(), "the write reached disk outside the project");

    let events = effects_turn(
        &mut set,
        &dir,
        tab,
        &format!(
            "Use the Bash tool to run exactly this command, unchanged: rm -rf {}",
            victim.display()
        ),
        "rm-rf-outside",
    );
    print_tool_io(&events, "Bash", "rm-rf-outside");
    assert_no_permission_requested(&events, "rm-rf-outside");
    assert_permission_resolved_allowed(&events, "rm-rf-outside");
    assert_tool_ran_without_error(&events, "Bash", "rm-rf-outside");
    assert!(!victim.exists(), "the directory was really removed outside the project");

    let events = effects_turn(
        &mut set,
        &dir,
        tab,
        &format!(
            "Use the Read tool to read the file at the exact absolute path {} and reply with exactly \
             its contents and nothing else.",
            read_target.display()
        ),
        "read-outside",
    );
    print_tool_io(&events, "Read", "read-outside");
    assert_no_permission_requested(&events, "read-outside");
    assert_permission_resolved_allowed(&events, "read-outside");
    assert_tool_ran_without_error(&events, "Read", "read-outside");
    assert!(
        transcript(&set, tab).contains("OUTSIDE-READ-OK"),
        "the model actually saw the outside file's contents"
    );
}

/// Test 1b: spec §2.4's latency bound, measured by driving `take_ui_delivery_with_rules` directly every
/// 1ms (bypassing the 33ms pump entirely) so the number is the host's own answer latency, not the
/// pump's tick. Five in-project `Read`s, one call per file.
///
/// **Paired by id, never by arrival order** (Codex v1-mode finding 2). The first version paired the
/// i-th `Read` `ToolCallStarted` with the i-th `PermissionResolved` of ANY tool: in bypass every
/// hooked call is answered (`ToolSearch`, which this CLI line calls before nearly everything, `Glob`,
/// ...), so one extra resolution ahead of sequential Reads gave negative latencies and a false PASS.
/// Now a resolution's `permission_id` is mapped to its request's `tool_use_id` (read off the
/// projection's pending requests on every pass, before and after the answering call -- bypass drops
/// the request from the delivery, so the projection is the only place it is seen), and only a `Read`'s
/// own resolution is paired with that `Read`'s own request. Exactly five pairs are required, every
/// latency must be non-negative, and every raw figure is printed first.
///
/// **The start marker is the request, not `ToolCallStarted`** (the TEST-profile rerun at `510c51c`): paired by
/// id, every `Read`'s `PermissionResolved` arrived 0-264 ms BEFORE its own `ToolCallStarted`, because the CLI
/// starts the tool only after the host answered -- so `ToolCallStarted` is the answer's consequence, not the
/// question. The host's answer latency is request-seen -> resolution-seen, per permission id.
#[test]
#[ignore = "real Claude; run under a test-account wrapper, see the module doc"]
fn host_answer_latency_in_bypass() {
    let label = "bypass-latency";
    let dir = project(label);
    for i in 1..=5 {
        std::fs::write(dir.join(format!("f{i}.txt")), format!("contents-{i}\n")).unwrap();
    }
    let mut backend = LiveBackend(
        AgentBackend::start(BackendKind::Sidecar, &dir, None)
            .map_err(|e| e.message)
            .expect("a sidecar session starts; is this running under a test-account wrapper?"),
    );
    backend
        .send_turn(
            "Use the Read tool to read each of f1.txt, f2.txt, f3.txt, f4.txt and f5.txt in the \
             current directory, one call per file, then reply with exactly DONE and nothing else.",
            "latency-reads",
        )
        .map_err(|e| e.message)
        .unwrap();

    /// Every request the projection holds right now, by permission id: its call's `tool_use_id` (the
    /// sidecar's proto3 wire can carry `""`, read as none) and its tool name. First sighting wins.
    fn note_requests(
        backend: &AgentBackend,
        into: &mut HashMap<String, (Option<String>, String)>,
        seen_at: &mut HashMap<String, f64>,
        at_ms: f64,
    ) {
        let projection = backend.projection();
        for (id, request) in projection.pending_permissions.iter() {
            seen_at.entry(id.clone()).or_insert(at_ms);
            into.entry(id.clone()).or_insert_with(|| {
                (
                    request.tool_use_id.clone().filter(|t| !t.is_empty()),
                    request.tool_name.clone(),
                )
            });
        }
    }

    let start = Instant::now();
    let rules = PrefixRules::default();
    let mut host_answered = BTreeSet::new();
    // tool_use_id -> when its `Read` started.
    let mut read_started: BTreeMap<String, f64> = BTreeMap::new();
    // permission_id -> (tool_use_id, tool name).
    let mut requests: HashMap<String, (Option<String>, String)> = HashMap::new();
    // permission_id -> when its request was first seen (the start marker).
    let mut requested_at: HashMap<String, f64> = HashMap::new();
    // permission_id -> (when its resolution arrived, outcome).
    let mut resolved: BTreeMap<String, (f64, PermissionOutcome)> = BTreeMap::new();
    let mut turn_ended = false;
    let deadline = Instant::now() + Duration::from_secs(240);
    while !turn_ended {
        let before_ms = start.elapsed().as_secs_f64() * 1000.0;
        note_requests(&backend, &mut requests, &mut requested_at, before_ms);
        let delivery = backend.take_ui_delivery_with_rules(&dir, &rules, PermissionMode::Bypass, &mut host_answered);
        let now_ms = start.elapsed().as_secs_f64() * 1000.0;
        note_requests(&backend, &mut requests, &mut requested_at, now_ms);
        if let agent::UiDelivery::Events(events) = delivery {
            for event in &events {
                match event {
                    AgentDomainEvent::ToolCallStarted { name, tool_use_id, .. } if name == "Read" => {
                        read_started.entry(tool_use_id.clone()).or_insert(now_ms);
                    }
                    // Only if bypass failed to answer it (then it is a card): recorded all the same.
                    AgentDomainEvent::PermissionRequested {
                        permission_id,
                        tool_use_id,
                        tool_name,
                        ..
                    } => {
                        requested_at.entry(permission_id.clone()).or_insert(now_ms);
                        requests
                            .entry(permission_id.clone())
                            .or_insert_with(|| (tool_use_id.clone().filter(|t| !t.is_empty()), tool_name.clone()));
                    }
                    AgentDomainEvent::PermissionResolved { permission_id, outcome } => {
                        resolved.entry(permission_id.clone()).or_insert((now_ms, *outcome));
                    }
                    AgentDomainEvent::TurnCompleted { .. } => turn_ended = true,
                    _ => {}
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out; read_started={read_started:?} requests={requests:?} resolved={resolved:?}"
        );
        std::thread::sleep(Duration::from_millis(1));
    }

    // (tool_use_id, started, resolved, outcome) for each `Read` whose own resolution arrived.
    let mut pairs: Vec<(String, f64, f64, PermissionOutcome)> = Vec::new();
    for (permission_id, (resolved_at, outcome)) in &resolved {
        let Some((Some(tool_use_id), tool_name)) = requests.get(permission_id) else {
            continue;
        };
        if tool_name != "Read" {
            continue;
        }
        if let (true, Some(requested)) = (read_started.contains_key(tool_use_id), requested_at.get(permission_id)) {
            pairs.push((tool_use_id.clone(), *requested, *resolved_at, *outcome));
        }
    }
    let other_resolutions: Vec<String> = resolved
        .keys()
        .filter(|id| requests.get(*id).is_none_or(|(_, tool)| tool != "Read"))
        .map(|id| format!("{id}={:?}", requests.get(id).map(|(_, tool)| tool.as_str())))
        .collect();
    println!(
        "[v1-mode] host_answer_latency_in_bypass raw: read_started={read_started:?} requests={requests:?} \
         requested_at={requested_at:?} resolved={resolved:?} pairs={pairs:?} \
         non-Read resolutions (excluded)={other_resolutions:?}"
    );

    assert_eq!(
        read_started.len(),
        5,
        "the prompt asks for five Reads, one per file; any other count is a run to repeat, not a figure"
    );
    assert_eq!(
        pairs.len(),
        5,
        "every Read must be paired with its OWN resolution, by id (Codex finding 2); unpaired Reads mean \
         the measurement is incomplete -- read the raw line above"
    );
    for (tool_use_id, _, _, outcome) in &pairs {
        assert_eq!(*outcome, PermissionOutcome::Allowed, "bypass allowed {tool_use_id}");
    }
    let mut latencies: Vec<f64> = pairs
        .iter()
        .map(|(_, started, resolved, _)| resolved - started)
        .collect();
    assert!(
        latencies.iter().all(|l| *l >= 0.0),
        "a Read's resolution was seen before its own request ({latencies:?}): the pairing is wrong -- report \
         it, do not record a figure"
    );
    latencies.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let min = latencies.first().copied().unwrap();
    let max = latencies.last().copied().unwrap();
    let median = latencies[latencies.len() / 2];
    println!("[v1-mode] host_answer_latency_in_bypass: n=5 min={min:.3}ms median={median:.3}ms max={max:.3}ms");
    assert!(
        median < 1000.0,
        "median host-answer latency in bypass must stay under 1s (spec §2.4): {median:.3}ms"
    );
}

/// Test 2: the same three calls as test 1, on `BackendKind::Legacy` (proves D4: `--permission-mode
/// default` + the hook's own `allow` still grants). Legacy's resolution is returned synchronously
/// and dropped with the request (`answer_what_needs_no_human`'s documented gap), so this asserts
/// less than test 1: no card, no tool error, the effect on disk -- never a `PermissionResolved`.
/// Only in a build that has the legacy backend (`--features legacy-backend`, spec
/// 2026-09-27-v1-dist-design.md §10, D16).
#[cfg(feature = "legacy-backend")]
#[test]
#[ignore = "real Claude; run under a test-account wrapper, see the module doc"]
fn legacy_default_mode_with_the_hook_allowing_runs_the_same_calls() {
    let label = "bypass-effects-legacy";
    let dir = project(label);
    let (mut set, tab) = bypass_tab(BackendKind::Legacy, &dir);

    let outside = scratch(label, "outside");
    let write_target = outside.join("w.txt");
    let victim = scratch(label, "victim");
    std::fs::write(victim.join("keep.txt"), "erase me").unwrap();
    // A plainly named file outside the project, not an `id_rsa`-shaped one: the auto policy cards any
    // Read outside the root either way, and the 2026-09-27 real run showed the model refusing to read a
    // file named `id_rsa` at all ("credential-exfiltration pattern"), which measured nothing about bypass.
    let read_dir = scratch(label, "outside-read");
    let read_target = read_dir.join("outside-notes.txt");
    std::fs::write(&read_target, "OUTSIDE-READ-OK\n").unwrap();

    let events = effects_turn(
        &mut set,
        &dir,
        tab,
        &format!(
            "Use the Write tool to create the file at the exact absolute path {} with the exact \
             content OUTSIDE-OK and nothing else.",
            write_target.display()
        ),
        "write-outside",
    );
    // Printed first: the last legacy run's `rm -rf` left the directory, and nothing said why.
    print_tool_io(&events, "Write", "write-outside");
    assert_no_permission_requested(&events, "write-outside");
    assert_tool_ran_without_error(&events, "Write", "write-outside");
    assert!(write_target.exists(), "the write reached disk outside the project");

    let events = effects_turn(
        &mut set,
        &dir,
        tab,
        &format!(
            "Use the Bash tool to run exactly this command, unchanged: rm -rf {}",
            victim.display()
        ),
        "rm-rf-outside",
    );
    // Printed first: the last legacy run's `rm -rf` left the directory, and nothing said why.
    print_tool_io(&events, "Bash", "rm-rf-outside");
    assert_no_permission_requested(&events, "rm-rf-outside");
    assert_tool_ran_without_error(&events, "Bash", "rm-rf-outside");
    assert!(!victim.exists(), "the directory was really removed outside the project");

    let events = effects_turn(
        &mut set,
        &dir,
        tab,
        &format!(
            "Use the Read tool to read the file at the exact absolute path {} and reply with exactly \
             its contents and nothing else.",
            read_target.display()
        ),
        "read-outside",
    );
    // Printed first: the last legacy run's `rm -rf` left the directory, and nothing said why.
    print_tool_io(&events, "Read", "read-outside");
    assert_no_permission_requested(&events, "read-outside");
    assert_tool_ran_without_error(&events, "Read", "read-outside");
    assert!(
        transcript(&set, tab).contains("OUTSIDE-READ-OK"),
        "the model actually saw the outside file's contents"
    );
}

/// Every line of this session's CLI transcripts, with the file it came from: the main
/// `<session>.jsonl` and each `<session>/subagents/agent-<id>.jsonl` (CLI 2.1.x writes a subagent's
/// turns there, every line `isSidechain: true` with its `agentId`). Found by walking the projects
/// directory for the session id rather than by re-deriving the CLI's cwd slug. The directory is
/// `agent::transcript::claude_projects_dir()`: under the test-account wrapper that is the TEST profile's
/// (`CLAUDE_CONFIG_DIR` for legacy's child, `VERDANDI_CLAUDE_ACCOUNT=test` for the sidecar's), the
/// same one. Test evidence only: the product never reads transcript content (`agent/src/transcript.rs`).
fn session_transcript_lines(provider_session_id: &str) -> Vec<(PathBuf, serde_json::Value)> {
    let Ok(projects) = agent::transcript::claude_projects_dir() else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = Vec::new();
    for bucket in std::fs::read_dir(&projects).into_iter().flatten().flatten() {
        let bucket = bucket.path();
        let main = bucket.join(format!("{provider_session_id}.jsonl"));
        if main.is_file() {
            files.push(main);
        }
        let subagents = bucket.join(provider_session_id).join("subagents");
        for file in std::fs::read_dir(&subagents).into_iter().flatten().flatten() {
            let file = file.path();
            if file.extension().is_some_and(|e| e == "jsonl") {
                files.push(file);
            }
        }
    }
    let mut lines = Vec::new();
    for file in files {
        for line in std::fs::read_to_string(&file).unwrap_or_default().lines() {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
                lines.push((file.clone(), value));
            }
        }
    }
    lines
}

/// The content blocks of one transcript line's message, or none.
fn content_blocks(line: &serde_json::Value) -> Vec<&serde_json::Value> {
    line["message"]["content"]
        .as_array()
        .map(|blocks| blocks.iter().collect())
        .unwrap_or_default()
}

/// Where the CLI recorded a `tool_use` block: `(isSidechain, agentId, file)`.
type Origin = (bool, Option<String>, PathBuf);

/// Where the CLI recorded the `tool_use` block with this id.
fn tool_use_origin(lines: &[(PathBuf, serde_json::Value)], tool_use_id: &str) -> Option<Origin> {
    lines.iter().find_map(|(file, line)| {
        content_blocks(line)
            .iter()
            .any(|b| b["type"] == "tool_use" && b["id"].as_str() == Some(tool_use_id))
            .then(|| {
                (
                    line["isSidechain"].as_bool().unwrap_or(false),
                    line["agentId"].as_str().map(str::to_string),
                    file.clone(),
                )
            })
    })
}

/// Every `Write` a subagent attempted (a sidechain `tool_use` named `Write`), with what the CLI
/// answered it: `(tool_use_id, agentId, is_error, result text)`.
fn sidechain_writes(lines: &[(PathBuf, serde_json::Value)]) -> Vec<(String, Option<String>, Option<bool>, String)> {
    let mut writes = Vec::new();
    for (_, line) in lines {
        if line["isSidechain"].as_bool() != Some(true) {
            continue;
        }
        for block in content_blocks(line) {
            if block["type"] == "tool_use" && block["name"] == "Write" {
                let id = block["id"].as_str().unwrap_or("").to_string();
                let result = lines.iter().find_map(|(_, other)| {
                    content_blocks(other)
                        .into_iter()
                        .find(|b| b["type"] == "tool_result" && b["tool_use_id"].as_str() == Some(id.as_str()))
                        .cloned()
                });
                writes.push((
                    id,
                    line["agentId"].as_str().map(str::to_string),
                    result.as_ref().and_then(|r| r["is_error"].as_bool()),
                    result
                        .map(|r| r["content"].to_string())
                        .unwrap_or_else(|| "<no tool_result>".into()),
                ));
            }
        }
    }
    writes
}

/// A `Write` card the probe saw, and whether a subagent call (`Agent`/`Task`) was open when it came.
#[derive(Debug)]
struct WriteCard {
    permission_id: String,
    tool_use_id: Option<String>,
    while_subagent_open: bool,
}

/// The subagent tool: `Agent` on CLI 2.1.283, `Task` on older builds.
fn is_subagent_tool(name: &str) -> bool {
    name == "Agent" || name == "Task"
}

/// Test 3: `auto`, both backends: does the `PreToolUse` gate see a SUBAGENT's own tool call, or does it
/// slip through unasked? Card + file: the gate sees subagents (bypass will answer them too). No card +
/// no file: the CLI's `default` refused an unhooked call (spec §2.5, a functional gap, an O3 trigger --
/// recorded, not failed). No card + file: a leak -- a security finding, and a failure.
///
/// **The Write must be shown to come from inside a subagent** (Codex v1-mode finding 4): a model that
/// ignores "do nothing else yourself" and writes `sub.txt` itself used to produce exactly the success
/// pair (`card_seen`, `sub.txt` exists) with no subagent path exercised. So the probe answers the
/// `Agent`/`Task` card `allow` when it arrives, requires that such a call really started, and counts a
/// `Write` card as the subagent's only when (a) it arrived while that call was still open and (b) the
/// CLI's own transcript records the `Write`'s `tool_use` id on a sidechain line with an `agentId`
/// (neither backend's wire carries a parent tool-use id or an agent id: `AgentDomainEvent` has none,
/// and Verdandi's proto has none). A run that cannot show that is inconclusive and fails, saying so,
/// rather than recording a result. Any other card is denied, so nothing else runs.
fn a_subagent_tool_call_reaches_the_host_on(kind: BackendKind, label: &str) {
    let dir = project(label);
    let (mut set, tab) = auto_tab(kind, &dir);
    let mut timeline: Vec<String> = Vec::new();
    let mut subagent_calls: Vec<String> = Vec::new();
    let mut open_subagent_calls: BTreeSet<String> = BTreeSet::new();
    let mut subagent_card_answered = false;
    let mut write_cards: Vec<WriteCard> = Vec::new();
    let mut denied: Vec<String> = Vec::new();
    let mut answered: BTreeSet<String> = BTreeSet::new();
    drive_turn(
        &mut set,
        &dir,
        tab,
        "Use the Agent tool (called Task on older versions) to launch exactly one subagent whose only \
         job is to use the Write tool to create a file named sub.txt in the current directory \
         containing the word ok. Do not use the Write tool yourself, and do nothing else yourself.",
        label,
        |set, new| {
            // (permission id, tool-use id, tool, delivered as a card rather than found pending).
            let mut asks: Vec<(String, Option<String>, String, bool)> = Vec::new();
            for event in new {
                let tool_use_id = event["tool_use_id"]
                    .as_str()
                    .filter(|t| !t.is_empty())
                    .map(str::to_string);
                match event["type"].as_str() {
                    Some("tool_call_started") => {
                        let name = event["name"].as_str().unwrap_or("");
                        timeline.push(format!("started {name} {tool_use_id:?}"));
                        if is_subagent_tool(name) {
                            if let Some(id) = tool_use_id {
                                subagent_calls.push(id.clone());
                                open_subagent_calls.insert(id);
                            }
                        }
                    }
                    Some("tool_call_completed") => {
                        timeline.push(format!("completed {tool_use_id:?} is_error={}", event["is_error"]));
                        if let Some(id) = tool_use_id {
                            open_subagent_calls.remove(&id);
                        }
                    }
                    Some("permission_requested") => {
                        let tool = event["tool_name"].as_str().unwrap_or("").to_string();
                        let permission_id = event["permission_id"].as_str().unwrap_or("").to_string();
                        timeline.push(format!("card {tool} {permission_id} for {tool_use_id:?}"));
                        asks.push((permission_id, tool_use_id, tool, true));
                    }
                    _ => {}
                }
            }
            // A `Write` or subagent card a `Resync` delivered only as a snapshot (`drive_turn` hands
            // those on as no events): whatever of those two is pending and not yet answered here.
            // Nothing else is taken from the projection -- on the sidecar a request the classifier
            // already allowed stays pending there until its resolution arrives, and must not be
            // answered (or denied) a second time.
            {
                let backend = set.get(tab).unwrap().live().unwrap();
                let projection = backend.projection();
                for request in projection.pending_permissions.values() {
                    let wanted = request.tool_name == "Write" || is_subagent_tool(&request.tool_name);
                    if wanted && !asks.iter().any(|(id, _, _, _)| id == &request.permission_id) {
                        asks.push((
                            request.permission_id.clone(),
                            request.tool_use_id.clone().filter(|t| !t.is_empty()),
                            request.tool_name.clone(),
                            false,
                        ));
                    }
                }
            }
            for (permission_id, tool_use_id, tool, delivered) in asks {
                if !answered.insert(permission_id.clone()) {
                    continue;
                }
                let decision = if is_subagent_tool(&tool) {
                    subagent_card_answered |= delivered;
                    PermissionDecision::Allow
                } else if tool == "Write" {
                    write_cards.push(WriteCard {
                        permission_id: permission_id.clone(),
                        tool_use_id,
                        while_subagent_open: !open_subagent_calls.is_empty(),
                    });
                    PermissionDecision::Allow
                } else {
                    denied.push(tool.clone());
                    PermissionDecision::Deny {
                        reason: Some("v1-mode probe: only the subagent and its Write are allowed".into()),
                    }
                };
                let result = set
                    .get_mut(tab)
                    .unwrap()
                    .live_mut()
                    .unwrap()
                    .respond_permission(&permission_id, decision);
                if let Err(e) = result {
                    timeline.push(format!("could not answer {tool} {permission_id}: {}", e.message));
                }
            }
        },
    );
    let sub_txt_exists = dir.join("sub.txt").exists();
    let provider_session_id = set
        .get(tab)
        .unwrap()
        .provider_session_id()
        .expect("a session that ran a turn has a Claude session id");
    // The transcript is the CLI's own file: give it a moment to be flushed after the turn's end.
    let mut lines = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    let transcript_has = |lines: &[(PathBuf, serde_json::Value)]| {
        write_cards
            .iter()
            .filter_map(|c| c.tool_use_id.as_deref())
            .all(|id| tool_use_origin(lines, id).is_some())
    };
    while Instant::now() < deadline {
        lines = session_transcript_lines(&provider_session_id);
        if !lines.is_empty() && transcript_has(&lines) {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let origins: Vec<(String, Option<Origin>)> = write_cards
        .iter()
        .map(|c| {
            let origin = c.tool_use_id.as_deref().and_then(|id| tool_use_origin(&lines, id));
            (c.permission_id.clone(), origin)
        })
        .collect();
    let attempts = sidechain_writes(&lines);
    // A subagent `Write` that RAN (its result is not an error) with no card for its tool-use id is a
    // leak whatever else happened in the turn -- another `Write` carding does not excuse it.
    let carded: BTreeSet<&str> = write_cards.iter().filter_map(|c| c.tool_use_id.as_deref()).collect();
    let ran_unasked: Vec<&(String, Option<String>, Option<bool>, String)> = attempts
        .iter()
        .filter(|(id, _, is_error, _)| !carded.contains(id.as_str()) && *is_error == Some(false))
        .collect();
    let from_subagent: Vec<&WriteCard> = write_cards
        .iter()
        .zip(&origins)
        .filter(|(card, (_, origin))| {
            card.while_subagent_open && matches!(origin, Some((true, Some(agent_id), _)) if !agent_id.is_empty())
        })
        .map(|(card, _)| card)
        .collect();
    println!(
        "[v1-mode] test3 subagent-write on {}: subagent_calls={subagent_calls:?} subagent_card_answered=\
         {subagent_card_answered} write_cards={write_cards:?} origins={origins:?} from_subagent={} \
         sub.txt_exists={sub_txt_exists} denied={denied:?} transcript_lines={} sidechain_write_attempts=\
         {attempts:?}",
        kind.as_str(),
        from_subagent.len(),
        lines.len()
    );
    println!("[v1-mode] test3 timeline on {}: {timeline:#?}", kind.as_str());

    assert!(
        ran_unasked.is_empty(),
        "SECURITY FINDING on {}: a subagent Write ran with no card ever drawn for it: {ran_unasked:?}",
        kind.as_str()
    );
    assert!(
        !subagent_calls.is_empty(),
        "INCONCLUSIVE on {}: the model never started an Agent/Task call, so no subagent path was exercised \
         -- rerun; this is not a result",
        kind.as_str()
    );
    if !write_cards.is_empty() {
        assert!(
            !from_subagent.is_empty(),
            "INCONCLUSIVE on {}: a Write card arrived, but none is shown to be the subagent's (open Agent/Task \
             call + a sidechain transcript line with an agentId) -- the main agent may have written it \
             itself; rerun, and do not record `card_seen` as the gate seeing subagents",
            kind.as_str()
        );
        println!(
            "[v1-mode] test3 on {}: the gate sees a subagent's Write (card answered, sub.txt_exists={sub_txt_exists})",
            kind.as_str()
        );
        return;
    }
    assert!(
        !sub_txt_exists,
        "SECURITY FINDING on {}: sub.txt was written with NO Write card ever drawn -- the gate does not see \
         this call at all (sidechain attempts: {attempts:?})",
        kind.as_str()
    );
    assert!(
        !attempts.is_empty(),
        "INCONCLUSIVE on {}: no Write card and no sub.txt, and the transcript shows no subagent Write attempt \
         either -- the subagent never tried; rerun",
        kind.as_str()
    );
    eprintln!(
        "[v1-mode] O3/§2.5 TRIGGER on {}: the subagent's Write reached neither a card nor the disk -- the \
         CLI's `default` mode refused a call the hook never saw; report this in concerns, do not loosen \
         anything. What the CLI answered: {attempts:?}",
        kind.as_str()
    );
}

#[test]
#[ignore = "real Claude; run under a test-account wrapper, see the module doc"]
fn a_subagent_tool_call_reaches_the_host_on_sidecar() {
    a_subagent_tool_call_reaches_the_host_on(BackendKind::Sidecar, "subagent-sidecar");
}

/// Only in a build that has the legacy backend (`--features legacy-backend`, spec
/// 2026-09-27-v1-dist-design.md §10, D16).
#[cfg(feature = "legacy-backend")]
#[test]
#[ignore = "real Claude; run under a test-account wrapper, see the module doc"]
fn a_subagent_tool_call_reaches_the_host_on_legacy() {
    a_subagent_tool_call_reaches_the_host_on(BackendKind::Legacy, "subagent-legacy");
}

/// Test 4 (O3, 2026-09-27): `Write` into `.claude/` and `.git/` inside a real git repo -- the CLI's own
/// sensitive-file check asks about each even after the gate allowed it, and a real
/// `bypassPermissions` session runs them. First recorded here as refused (the O3 trigger); since
/// Verdandi b3aa188 routes that ask to the host (`provider_permission_prompts`), neovibe answers it.
///
/// - **Bypass** (O3 ruling 4): both writes SUCCEED with no card -- the file is on disk, the tool
///   result is not an error, nothing reached the panel as a request.
/// - **Auto** (ruling 5): a `Write` to `.git/probe2` draws exactly ONE card (the gate's); the human
///   approves it (`TabSet::answer_card`, the panel's own route) and the CLI's own prompt for the same
///   call is then answered without a second card; the file is written. At least one allowed
///   resolution names a permission id that was never delivered -- that is the CLI's own prompt,
///   answered by the host, so the mechanism really ran rather than the CLI not asking at all.
///
/// Needs a sidecar advertising `provider_permission_prompts` (b3aa188 or later):
/// `NEOVIBE_SIDECAR_BINARY=<that artifact>`.
#[test]
#[ignore = "real Claude; run under a test-account wrapper, see the module doc"]
fn protected_paths_under_default_with_hook_allow() {
    let label = "protected-paths";
    let dir = project(label);
    let status = std::process::Command::new("git")
        .arg("init")
        .arg("-q")
        .current_dir(&dir)
        .status()
        .expect("git must be on PATH for this probe");
    assert!(status.success(), "git init failed");

    let (mut set, tab) = bypass_tab(BackendKind::Sidecar, &dir);
    let advertised = set.get(tab).unwrap().live().unwrap().provider_info().map(|info| {
        info.advertised_capabilities
            .iter()
            .any(|c| c == "provider_permission_prompts")
    });
    assert_eq!(
        advertised,
        Some(true),
        "this probe needs a sidecar with provider_permission_prompts (Verdandi b3aa188+)"
    );

    for rel in [".claude/probe.json", ".git/probe"] {
        let target = dir.join(rel);
        let prompt = format!(
            "Use the Write tool to create the file at the exact relative path {rel} with the exact \
             content PROBE and nothing else."
        );
        let mut is_error: Option<bool> = None;
        let mut result_text = String::new();
        let mut requested = 0usize;
        let mut allowed = 0usize;
        drive_turn(&mut set, &dir, tab, &prompt, rel, |_set, new| {
            for event in new {
                if event["type"] == "tool_call_completed" {
                    is_error = event["is_error"].as_bool();
                    result_text = event["content"].to_string();
                }
                if event["type"] == "permission_requested" {
                    requested += 1;
                }
                if event["type"] == "permission_resolved" && event["outcome"] == "allowed" {
                    allowed += 1;
                }
            }
        });
        let ran = target.exists();
        println!(
            "[v1-mode] test4 bypass write {rel}: ran={ran} is_error={is_error:?} requests_delivered={requested} \
             allowed_resolutions={allowed} result={result_text}"
        );
        assert!(
            ran,
            "O3: bypass must run a Write to {rel} as a real bypassPermissions session does; the tool \
             result was {result_text}"
        );
        assert_eq!(is_error, Some(false), "{rel}: {result_text}");
        assert_eq!(
            std::fs::read_to_string(&target).unwrap().trim(),
            "PROBE",
            "{rel}: the content the prompt asked for"
        );
        assert_eq!(
            requested, 0,
            "{rel}: bypass draws no card, the CLI's own prompt included"
        );
    }

    // Auto, same session: leaving bypass is immediate (D6).
    match set.cycle_mode(tab).unwrap() {
        ModeCycle::Changed(SessionModeChoice::Auto) => {}
        other => panic!("leaving bypass moves at once (D6), got {other:?}"),
    }
    let rel = ".git/probe2";
    let target = dir.join(rel);
    let mut cards: Vec<(String, String)> = Vec::new();
    let mut allowed: Vec<String> = Vec::new();
    let mut is_error: Option<bool> = None;
    let mut result_text = String::new();
    drive_turn(
        &mut set,
        &dir,
        tab,
        &format!(
            "Use the Write tool to create the file at the exact relative path {rel} with the exact content \
             PROBE2 and nothing else."
        ),
        rel,
        |set, new| {
            for event in new {
                if event["type"] == "permission_requested" {
                    let id = event["permission_id"].as_str().unwrap().to_string();
                    let tool = event["tool_name"].as_str().unwrap_or_default().to_string();
                    cards.push((id.clone(), tool));
                    // The human's Approve, through the panel's own route.
                    set.answer_card(tab, &id, PermissionDecision::Allow)
                        .map_err(|e| e.message)
                        .unwrap();
                }
                if event["type"] == "permission_resolved" && event["outcome"] == "allowed" {
                    allowed.push(event["permission_id"].as_str().unwrap().to_string());
                }
                if event["type"] == "tool_call_completed" {
                    is_error = event["is_error"].as_bool();
                    result_text = event["content"].to_string();
                }
            }
        },
    );
    let delivered: BTreeSet<&str> = cards.iter().map(|(id, _)| id.as_str()).collect();
    let host_answered: Vec<&String> = allowed.iter().filter(|id| !delivered.contains(id.as_str())).collect();
    println!(
        "[v1-mode] test4 auto write {rel}: ran={} is_error={is_error:?} cards={cards:?} \
         allowed_resolutions={allowed:?} answered_by_the_host={host_answered:?} result={result_text}",
        target.exists()
    );
    assert_eq!(
        cards.iter().filter(|(_, tool)| tool == "Write").count(),
        1,
        "exactly one card for the Write -- the CLI's own prompt that follows the human's approval is \
         answered without a second one: {cards:?}"
    );
    assert!(target.exists(), "the approved write reached disk: {result_text}");
    assert_eq!(is_error, Some(false), "{rel}: {result_text}");
    assert!(
        !host_answered.is_empty(),
        "an allowed resolution for a request never drawn -- the CLI's own prompt -- must show the \
         mechanism ran: cards={cards:?} allowed={allowed:?}"
    );
}

/// Test 5 (D12): a project's own `permissions.defaultMode` must never leave a session ungated. Each of
/// the four combinations must land in exactly one of two branches: `Gated` (a card arrives; deny it;
/// nothing written) or `Tripped` (D12 closes the session; nothing written). `inside.txt` existing
/// without a card ever having been answered `Allow` is never acceptable in either branch, and is
/// asserted as such regardless of which one this run took. On legacy the explicit
/// `--permission-mode default` must always win: the branch there must be `Gated`.
enum DefaultModeBranch {
    Gated,
    Tripped,
}

fn run_default_mode_probe(kind: BackendKind, default_mode: &str, label: &str) {
    let dir = project(label);
    std::fs::create_dir_all(dir.join(".claude")).unwrap();
    std::fs::write(
        dir.join(".claude/settings.json"),
        format!("{{\"permissions\":{{\"defaultMode\":\"{default_mode}\"}}}}"),
    )
    .unwrap();

    let mut set = LiveSet(TabSet::new(kind, SessionModeChoice::Auto));
    let tab = set.active();
    let backend = AgentBackend::start(kind, &dir, None)
        .map_err(|e| e.message)
        .expect("a session starts; is this running under a test-account wrapper?");
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);

    // The trip can fire from a `SessionReady` report alone, before any turn at all (mirrors the
    // sidecar shape of the unit test `a_cli_reporting_an_ungated_mode_closes_the_session`) -- give it
    // a short window on its own first, rather than assuming a turn is what triggers it.
    let early_deadline = Instant::now() + Duration::from_secs(15);
    let mut branch = None;
    while branch.is_none() && Instant::now() < early_deadline {
        let out = set.pump(&dir, true);
        if !out.tripped.is_empty() {
            for mut t in out.tripped {
                t.backend.shutdown();
            }
            branch = Some(DefaultModeBranch::Tripped);
        }
        std::thread::sleep(Duration::from_millis(33));
    }

    let branch = match branch {
        Some(b) => b,
        None => {
            set.get_mut(tab)
                .unwrap()
                .live_mut()
                .unwrap()
                .send_turn(
                    "Use the Write tool to create a file named inside.txt in the current directory \
                     containing the word ok.",
                    label,
                )
                .map_err(|e| e.message)
                .unwrap();

            let mut permission_id: Option<String> = None;
            let deadline = Instant::now() + Duration::from_secs(240);
            let mut found = loop {
                let out = set.pump(&dir, true);
                if !out.tripped.is_empty() {
                    for mut t in out.tripped {
                        t.backend.shutdown();
                    }
                    break DefaultModeBranch::Tripped;
                }
                for event in events_from_payload(out.active_payload.as_deref()) {
                    if event["type"] == "permission_requested" && permission_id.is_none() {
                        permission_id = event["permission_id"].as_str().map(str::to_string);
                    }
                }
                if permission_id.is_some() {
                    break DefaultModeBranch::Gated;
                }
                assert!(
                    Instant::now() < deadline,
                    "{label}: neither a card nor a trip arrived in time"
                );
                std::thread::sleep(Duration::from_millis(33));
            };

            if matches!(found, DefaultModeBranch::Gated) {
                let permission_id = permission_id.expect("Gated branch always sets it");
                set.get_mut(tab)
                    .unwrap()
                    .live_mut()
                    .unwrap()
                    .respond_permission(
                        &permission_id,
                        PermissionDecision::Deny {
                            reason: Some("v1-mode D12 probe".into()),
                        },
                    )
                    .map_err(|e| e.message)
                    .unwrap();
                let deadline = Instant::now() + Duration::from_secs(60);
                loop {
                    let out = set.pump(&dir, true);
                    if !out.tripped.is_empty() {
                        for mut t in out.tripped {
                            t.backend.shutdown();
                        }
                        // Codex v1-mode finding 3: a trip AFTER the card is still a trip. It used to
                        // leave `found` at `Gated`, so the Tripped checks below were skipped and the
                        // run printed `branch=gated` although the CLI had reported an ungated mode.
                        println!("[v1-mode] test5 {label}: the tripwire fired AFTER the card was answered");
                        found = DefaultModeBranch::Tripped;
                        break;
                    }
                    if !set.get(tab).unwrap().turn_running() {
                        break;
                    }
                    assert!(Instant::now() < deadline, "{label}: the denied turn never ended");
                    std::thread::sleep(Duration::from_millis(33));
                }
            }
            found
        }
    };

    let branch_name = match branch {
        DefaultModeBranch::Gated => "gated",
        DefaultModeBranch::Tripped => "tripped",
    };
    println!(
        "[v1-mode] test5 {label} (defaultMode={default_mode}, backend={}): branch={branch_name}",
        kind.as_str()
    );

    // The plan's Gated definition is "a card arrived AND no UngatedCliMode was reported": check the
    // second half too, including a report the ingestion folded that no pump has turned into a trip.
    if let DefaultModeBranch::Gated = branch {
        let state = set.get(tab).unwrap();
        let backend = state.live().unwrap_or_else(|| {
            panic!(
                "{label}: a gated tab keeps its session, but it is {:?}",
                state.wire_state()
            )
        });
        let reported = backend.projection().ungated_cli_mode.clone();
        assert!(
            reported.is_none(),
            "{label}: gated means no ungated mode was ever reported, but the CLI reported {reported:?}"
        );
    }

    if let DefaultModeBranch::Tripped = branch {
        let state = set.get(tab).unwrap();
        let wire_state = state.wire_state();
        let TabBackend::Failed { reason } = &state.backend else {
            panic!("{label}: a tripped tab must be Failed but was {wire_state:?}");
        };
        println!("[v1-mode] test5 {label}: [permission] {reason}");
        assert!(
            reason.contains("permissions.defaultMode"),
            "{label}: the tripwire's reason should name the setting: {reason}"
        );
    }

    assert!(
        !dir.join("inside.txt").exists(),
        "{label}: never acceptable -- inside.txt must not exist without a card ever answered Allow"
    );

    if kind == BackendKind::Legacy {
        assert!(
            matches!(branch, DefaultModeBranch::Gated),
            "{label}: D4 says the explicit --permission-mode default must win over the project's own \
             defaultMode on legacy, so this must be the Gated branch"
        );
    } else if matches!(branch, DefaultModeBranch::Tripped) {
        eprintln!(
            "[v1-mode] O3 note: {label} tripped the D12 tripwire rather than staying gated -- worth \
             reporting even though D12 says this is the correct fail-closed behaviour, not a bug"
        );
    }
}

#[test]
#[ignore = "real Claude; run under a test-account wrapper, see the module doc"]
fn default_mode_bypass_permissions_gates_the_sidecar() {
    run_default_mode_probe(BackendKind::Sidecar, "bypassPermissions", "default-mode-bypass-sidecar");
}

/// Only in a build that has the legacy backend (`--features legacy-backend`, spec
/// 2026-09-27-v1-dist-design.md §10, D16).
#[cfg(feature = "legacy-backend")]
#[test]
#[ignore = "real Claude; run under a test-account wrapper, see the module doc"]
fn default_mode_bypass_permissions_gates_legacy() {
    run_default_mode_probe(BackendKind::Legacy, "bypassPermissions", "default-mode-bypass-legacy");
}

#[test]
#[ignore = "real Claude; run under a test-account wrapper, see the module doc"]
fn default_mode_accept_edits_gates_the_sidecar() {
    run_default_mode_probe(BackendKind::Sidecar, "acceptEdits", "default-mode-accept-edits-sidecar");
}

/// Only in a build that has the legacy backend (`--features legacy-backend`, spec
/// 2026-09-27-v1-dist-design.md §10, D16).
#[cfg(feature = "legacy-backend")]
#[test]
#[ignore = "real Claude; run under a test-account wrapper, see the module doc"]
fn default_mode_accept_edits_gates_legacy() {
    run_default_mode_probe(BackendKind::Legacy, "acceptEdits", "default-mode-accept-edits-legacy");
}

/// Test 6: `auto`: a `Write` cards; `cycle_mode` returns a `Confirm` plan listing exactly that one waiting
/// card; `confirm_bypass` with its nonce approves it and the write lands; `cycle_mode` again leaves
/// bypass at once (D6); a second `Write` cards again on the now-auto tab (denied, to leave nothing
/// behind).
#[test]
#[ignore = "real Claude; run under a test-account wrapper, see the module doc"]
fn entering_bypass_mid_turn_approves_the_waiting_card() {
    let label = "mid-turn-bypass";
    let dir = project(label);
    let (mut set, tab) = auto_tab(BackendKind::Sidecar, &dir);

    set.get_mut(tab)
        .unwrap()
        .live_mut()
        .unwrap()
        .send_turn(
            "Use the Write tool to create a file named first.txt in the current directory containing \
             the word ok.",
            "first-write",
        )
        .map_err(|e| e.message)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        set.pump(&dir, true);
        if set.get(tab).unwrap().attention.attention().pending >= 1 {
            break;
        }
        assert!(Instant::now() < deadline, "the first Write card never arrived");
        std::thread::sleep(Duration::from_millis(33));
    }

    let plan = match set.cycle_mode(tab).unwrap() {
        ModeCycle::Confirm(plan) => plan,
        other => panic!("expected Confirm with the waiting card listed, got {other:?}"),
    };
    assert_eq!(plan.approve.len(), 1, "the plan lists exactly the one waiting card");

    match set.confirm_bypass(plan.scope, plan.nonce).unwrap() {
        ConfirmOutcome::Entered { approved, .. } => assert_eq!(approved, 1, "the listed card was approved"),
        other => panic!("expected Entered{{approved: 1}}, got {other:?}"),
    }

    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        set.pump(&dir, true);
        if !set.get(tab).unwrap().turn_running() {
            break;
        }
        assert!(Instant::now() < deadline, "the approved write never finished");
        std::thread::sleep(Duration::from_millis(33));
    }
    assert!(
        dir.join("first.txt").exists(),
        "the card the confirm listed was really approved"
    );

    match set.cycle_mode(tab).unwrap() {
        ModeCycle::Changed(SessionModeChoice::Auto) => {}
        other => panic!("leaving bypass moves at once (D6), got {other:?}"),
    }
    assert_eq!(set.get(tab).unwrap().mode(), SessionModeChoice::Auto);

    // A plain `effects_turn` would deadlock here: with the tab back in `auto`, this `Write` cards and
    // the turn cannot end until something answers it, so the card has to be denied AS the event
    // arrives, inside `drive_turn`'s own loop, not after it returns.
    let mut card_seen = false;
    drive_turn(
        &mut set,
        &dir,
        tab,
        "Use the Write tool to create a file named second.txt in the current directory containing the \
         word ok.",
        "second-write",
        |set, new| {
            for event in new {
                if event["type"] == "permission_requested" {
                    if let Some(id) = event["permission_id"].as_str() {
                        if !card_seen {
                            card_seen = true;
                            let _ = set.get_mut(tab).unwrap().live_mut().unwrap().respond_permission(
                                id,
                                PermissionDecision::Deny {
                                    reason: Some("v1-mode probe".into()),
                                },
                            );
                        }
                    }
                }
            }
        },
    );
    assert!(
        card_seen,
        "a second Write cards again now that the tab is back in auto (D6)"
    );
    assert!(!dir.join("second.txt").exists(), "denied: nothing written");
}
