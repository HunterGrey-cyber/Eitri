//! Real-CLI probes of the gated bypass (v1 mode plan Task 5, spec §2.4/§2.5, O3). `#[ignore]`d: it
//! spawns real Claude sessions and bills the account they run as.
//!
//! **Run only on the TEST profile, through the resolved binary, with a scratch state home:**
//!
//! ```sh
//! claude --version    # record the build; nothing prints it for you
//! XDG_STATE_HOME=$HOME/.cache/nv-v1mode-t5/state \
//!     cargo test -p eitri-core --test v1_mode_real_cli -- --ignored --nocapture --test-threads=1
//! ```
//!
//! The six legacy tests (2, 3 on legacy, 5 on legacy twice, 6b) exist only in a build with the legacy
//! backend: add `--features legacy-backend` to run them (spec 2026-09-27-v1-dist-design.md §10, D16).
//! They spawn `agent-hook`, a binary of the `agent` package that only a build with that feature
//! produces, and `cargo test -p eitri-core` does not build another package's binaries: build it first
//! (`cargo build -p agent --features legacy-backend --bin agent-hook`) or name `-p agent` in the same
//! `cargo test` invocation, or the legacy arms fail with "agent-hook binary not found" before any
//! turn runs.
//!
//! The test-account wrapper sets `VERDANDI_CLAUDE_CLI_PATH` to the `claude-wrapper` launcher, which is what
//! decides the sidecar's CLI (CLAUDE.md, the environment table); `PATH` alone does not.
//!
//! **What these prove, and what they only record.** R07/S2 say bypass is Eitri's own `allow`
//! under a CLI that always runs gated -- never `bypassPermissions` on the wire. Tests 1/1b/2/6 assert
//! that: no `PermissionRequested` a bypass tab answers itself is ever delivered to the panel, the
//! resolution still arrives (sidecar) or is silently dropped with the request (legacy, D4's own
//! documented gap), and the effect lands on disk. Tests 3, 4 and 5 are the probes spec O3 asks for --
//! whether the real CLI's own `default` mode plus a hook that always allows behaves exactly like
//! `bypassPermissions` would. **A refusal there is not this test suite's bug to fix**: per O3, it must
//! be recorded verbatim (the tool result text, the CLI build) and reported in the task's `concerns`,
//! never loosened or skipped around.
//!
//! **Auto tabs answer differently depending on the CLI.** On a sidecar that offers `cli_auto_mode` and
//! `permission_defer` the CLI runs its own `auto` and the host answers every gate request `defer`:
//! an in-project `Write` draws no card, its resolution arrives as `deferred`, and the CLI's classifier
//! decides. On legacy (and on a sidecar without those capabilities) the host's own policy answers an
//! in-project `Write` `allow` through the acceptEdits fast path, and says so only as a note on the
//! call's row (`autoNotes` in the panel's payload; `events_from_payload` turns each note into an
//! `answered_for_you` event). A test that needs a card in an Auto tab therefore uses a call both
//! worlds card -- a project `permissions.ask` rule, or a path outside the project -- and the tests
//! about "nothing runs ungated" accept an answer the host gave itself ([`GateTrace`]) as proof that
//! the gate saw the call, where they used to look only for a card. A card an ask rule forces is the
//! human's in every mode, and so is one that explains nothing, which is how a content-scoped ask rule
//! reaches the host on CLI 2.1.288, so entering bypass leaves either waiting and a bypass tab cards
//! them (test 6a); a card that entering bypass does approve needs the host's own policy to have raised
//! it, which only legacy does (test 6b).
//!
//! **A run that did not exercise what a test is about fails as INCONCLUSIVE** (v1-mode fix round 1,
//! after Codex's findings 2-4): test 1b needs exactly five Reads each paired with its own resolution
//! by id; test 3 needs an `Agent`/`Task` call and, for a `Write` card, proof from the CLI's own
//! transcript that a subagent made it; test 5 counts a trip after the card as a trip. Read the raw
//! lines each prints before recording any figure or branch.

use agent::{AgentDomainEvent, PermissionDecision, PermissionMode, PermissionOutcome, PrefixRules};
use eitri_core::agent_backend::{AgentBackend, BackendKind, GateOrigin};
use eitri_core::agent_bridge::SessionModeChoice;
use eitri_core::tab_set::{ConfirmOutcome, ModeCycle, TabBackend, TabSet};
use eitri_core::tabs::TabId;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
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
    // Trusted: this measures the real CLI with the project tiers loaded, as it always has.
    let backend = AgentBackend::start(kind, dir, None, agent::setting_sources::ProjectTrust::Trusted)
        .map_err(|e| e.message)
        .expect("a session starts; is this running under a test-account wrapper?");
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    (LiveSet(set), tab)
}

/// A tab in the ordinary `auto` default, with a live backend already attached.
fn auto_tab(kind: BackendKind, dir: &std::path::Path) -> (LiveSet, TabId) {
    let mut set = TabSet::new(kind, SessionModeChoice::Auto);
    let tab = set.active();
    let backend = AgentBackend::start(kind, dir, None, agent::setting_sources::ProjectTrust::Trusted)
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
///
/// The payload's row notes follow the events as synthetic `{"type": "answered_for_you", "tool_use_id":
/// ..}` entries: they are the host's own record of a call it answered `allow` without a card (the
/// edit fast path, a saved rule), and on legacy the only one, since that backend's synchronous
/// resolution is dropped with the request. A note arrives with the call's completion, so it can come
/// a few ticks after the answer itself. A note for the CLI's own prompt is a different question from
/// the gate's and comes as `{"type": "answered_prompt", ..}`, which no test takes as the gate seeing
/// a call.
fn events_from_payload(payload: Option<&str>) -> Vec<serde_json::Value> {
    let Some(payload) = payload else { return Vec::new() };
    let value: serde_json::Value = serde_json::from_str(payload).unwrap();
    if value["kind"] != "events" {
        return Vec::new();
    }
    let mut events = value["events"].as_array().cloned().unwrap_or_default();
    let noted = value["autoNotes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|id| id.as_str())
        .chain(
            value["ruleNotes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|note| note["toolUseId"].as_str()),
        );
    for id in noted {
        events.push(serde_json::json!({ "type": "answered_for_you", "tool_use_id": id }));
    }
    for note in value["promptNotes"].as_array().into_iter().flatten() {
        if let Some(id) = note["toolUseId"].as_str() {
            events.push(serde_json::json!({ "type": "answered_prompt", "tool_use_id": id, "note": note["note"] }));
        }
    }
    events
}

/// What the gate saw in a turn and who answered it. It exists so a test can tell "the host answered
/// this call itself" (an `allow` or a `defer` it gave, which the panel never draws) from "nobody was
/// asked": the second is a call that bypassed the gate, the first is not.
///
/// **Every conclusion is about one call, found by its tool, its input and its own tool-use id.** An
/// answer, a card, a refusal or a success belonging to some other call -- another `Write`, an earlier
/// call of the same turn, a subagent's -- proves nothing about the call under test, so nothing here
/// is a count or an "any" over the turn.
///
/// The host's answers come from the tab's own record of them (`Tab::gate_answers`), written where
/// each answer is sent. Only an answer to the gate's own request counts as the gate seeing a call: the
/// CLI's own permission prompt, which follows an allowed call, is recorded too but kept apart
/// (`prompt_answered`), since a call the gate never saw can still raise one. Reading the session's
/// pending requests instead would miss a request whose resolution folded on the ingestion thread
/// before the read, and such a miss leaves no other trace.
struct GateTrace {
    /// The project directory: a relative path an input names is read against it.
    root: PathBuf,
    /// Every gate request the host answered itself: (permission id, tool-use id if named, tool).
    answered: Vec<(String, Option<String>, String)>,
    /// Tool-use ids of calls whose CLI prompt (not the gate's request) the host answered.
    prompt_answered: BTreeSet<String>,
    /// Tool-use id -> the row note of the CLI's own prompt the host answered for that call
    /// ("Claude Code asked -- allowed in bypass"): its first words say whether the prompt gave a reason.
    prompt_notes: BTreeMap<String, String>,
    /// Tool-use id -> tool name, for every call that started.
    calls: BTreeMap<String, String>,
    /// Tool-use id -> the input the call started with.
    call_inputs: BTreeMap<String, serde_json::Value>,
    /// Tool-use id -> (the `is_error` of the call's completion, its content), for every call that
    /// completed.
    completed: BTreeMap<String, (Option<bool>, String)>,
    /// Tool-use ids of the calls a card was drawn for from the gate's own request.
    carded_calls: BTreeSet<String>,
    /// Tool-use ids of the calls a card was drawn for from the CLI's own prompt.
    prompt_carded_calls: BTreeSet<String>,
    /// Permission id -> outcome, for every resolution delivered.
    resolved: BTreeMap<String, String>,
    /// Tool-use ids the host noted on the row as answered without a card.
    noted: BTreeSet<String>,
}

/// How the gate saw one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GateSaw {
    /// The host answered the request itself (an `allow` or a `defer`).
    HostAnswer,
    /// A card was drawn for it.
    Card,
}

/// Whether `path`, as an input named it (absolute, or relative to `root`), is the file `target`
/// names under `root`. Compared as paths, so another file with the same name elsewhere is not it.
fn names_the_path(path: &str, root: &Path, target: &Path) -> bool {
    let path = Path::new(path);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let target = if target.is_absolute() {
        target.to_path_buf()
    } else {
        root.join(target)
    };
    if path == target {
        return true;
    }
    // The model may spell the directory through a link or with a `.`: the parent resolves the same.
    match (
        path.parent().map(Path::canonicalize),
        path.file_name(),
        target.parent().map(Path::canonicalize),
        target.file_name(),
    ) {
        (Some(Ok(a)), Some(x), Some(Ok(b)), Some(y)) => a == b && x == y,
        _ => false,
    }
}

impl GateTrace {
    fn new(root: &Path) -> Self {
        GateTrace {
            root: root.to_path_buf(),
            answered: Vec::new(),
            prompt_answered: BTreeSet::new(),
            prompt_notes: BTreeMap::new(),
            calls: BTreeMap::new(),
            call_inputs: BTreeMap::new(),
            completed: BTreeMap::new(),
            carded_calls: BTreeSet::new(),
            prompt_carded_calls: BTreeSet::new(),
            resolved: BTreeMap::new(),
            noted: BTreeSet::new(),
        }
    }

    fn observe(&mut self, set: &TabSet, tab: TabId, events: &[serde_json::Value]) {
        if let Some(tab) = set.get(tab) {
            self.record_answers(tab.gate_answers());
        }
        self.observe_events(events);
    }

    /// Takes the tab's whole record of the host's answers (it only grows, so this replaces).
    fn record_answers(&mut self, answers: &[eitri_core::agent_backend::GateAnswer]) {
        let (gate, prompt): (Vec<_>, Vec<_>) = answers.iter().partition(|a| a.origin == GateOrigin::GateRequest);
        self.answered = gate
            .iter()
            .map(|a| (a.permission_id.clone(), a.tool_use_id.clone(), a.tool_name.clone()))
            .collect();
        self.prompt_answered = prompt.iter().filter_map(|a| a.tool_use_id.clone()).collect();
    }

    fn observe_events(&mut self, events: &[serde_json::Value]) {
        for event in events {
            let tool_use_id = event["tool_use_id"]
                .as_str()
                .filter(|t| !t.is_empty())
                .map(str::to_string);
            match event["type"].as_str() {
                Some("tool_call_started") => {
                    if let Some(id) = tool_use_id {
                        self.call_inputs.insert(id.clone(), event["input"].clone());
                        self.calls
                            .insert(id, event["name"].as_str().unwrap_or_default().to_string());
                    }
                }
                Some("tool_call_completed") => {
                    if let Some(id) = tool_use_id {
                        self.completed
                            .insert(id, (event["is_error"].as_bool(), event["content"].to_string()));
                    }
                }
                Some("permission_requested") => {
                    if let Some(id) = tool_use_id {
                        // The CLI's own prompt is a separate question from the gate's request: a call
                        // the gate never saw can still raise one, so it is no evidence of the gate.
                        if event["provider_prompt"].is_object() {
                            self.prompt_carded_calls.insert(id);
                        } else {
                            self.carded_calls.insert(id);
                        }
                    }
                }
                Some("permission_resolved") => {
                    if let Some(id) = event["permission_id"].as_str() {
                        self.resolved
                            .entry(id.to_string())
                            .or_insert_with(|| event["outcome"].as_str().unwrap_or_default().to_string());
                    }
                }
                Some("answered_prompt") => {
                    if let Some(id) = tool_use_id {
                        self.prompt_notes
                            .insert(id, event["note"].as_str().unwrap_or_default().to_string());
                    }
                }
                Some("answered_for_you") => {
                    if let Some(id) = tool_use_id {
                        self.noted.insert(id);
                    }
                }
                _ => {}
            }
        }
    }

    /// The calls the host answered itself without a card: `(tool-use id if known, tool)`. A card a
    /// human approved is not here; a call the host noted on its row but whose answer was never
    /// recorded (it should not happen) is added, so a note alone still counts.
    fn host_answered(&self) -> Vec<(Option<String>, String)> {
        let mut answered: Vec<(Option<String>, String)> = self
            .answered
            .iter()
            .map(|(_, call, tool)| (call.clone(), tool.clone()))
            .collect();
        for id in &self.noted {
            let tool = self.calls.get(id).cloned().unwrap_or_else(|| "?".to_string());
            if !answered.iter().any(|(known, _)| known.as_deref() == Some(id.as_str())) {
                answered.push((Some(id.clone()), tool));
            }
        }
        answered
    }

    fn host_answered_ids(&self) -> BTreeSet<String> {
        self.host_answered().into_iter().filter_map(|(id, _)| id).collect()
    }

    /// How the gate saw the call `tool_use_id`: a host answer or a card for that very call, or neither.
    fn gate_saw(&self, tool_use_id: &str) -> Option<GateSaw> {
        if self.host_answered_ids().contains(tool_use_id) {
            Some(GateSaw::HostAnswer)
        } else if self.carded_calls.contains(tool_use_id) {
            Some(GateSaw::Card)
        } else {
            None
        }
    }

    /// The permission id of the gate request the host answered for the call `tool_use_id`, if it did.
    fn gate_permission_id(&self, tool_use_id: &str) -> Option<&str> {
        self.answered
            .iter()
            .find(|(_, call, _)| call.as_deref() == Some(tool_use_id))
            .map(|(permission_id, _, _)| permission_id.as_str())
    }

    /// Every call of `tool` that started and whose input satisfies `wanted`: `(tool-use id, is_error of
    /// its completion if it completed)`.
    fn calls_where(&self, tool: &str, wanted: impl Fn(&serde_json::Value) -> bool) -> Vec<(String, Option<bool>)> {
        self.calls
            .iter()
            .filter(|(_, name)| name.as_str() == tool)
            .filter(|(id, _)| wanted(&self.call_inputs[*id]))
            .map(|(id, _)| (id.clone(), self.completed.get(id).and_then(|(is_error, _)| *is_error)))
            .collect()
    }

    /// Every `Write` call that started and named `target` (a path relative to the project, such as
    /// `.git/probe2`, or an absolute one): `(tool-use id, is_error of its completion)`.
    fn writes_to(&self, target: &Path) -> Vec<(String, Option<bool>)> {
        self.calls_where("Write", |input| {
            input["file_path"]
                .as_str()
                .is_some_and(|path| names_the_path(path, &self.root, target))
        })
    }

    /// Checks the gate against the calls that wrote `target`, each by its own tool-use id: every
    /// write that did not fail outright (a completion that is an error excuses it; one that never
    /// completed does not) must have been seen by the gate (a host answer or a card for that call),
    /// and a target that exists on disk must be the work of at least one write that succeeded through
    /// the gate. An answer to some other call, even another `Write`, proves nothing about these.
    /// Returns how the gate saw each write that started, for the caller to require a particular kind.
    fn assert_target_writes_were_gated(&self, target: &str, exists: bool, label: &str) -> Vec<GateSaw> {
        let writes = self.writes_to(Path::new(target));
        let mut seen = Vec::new();
        let mut gated_successes = 0usize;
        for (id, is_error) in &writes {
            let saw = self.gate_saw(id);
            if is_error != &Some(true) {
                assert!(
                    saw.is_some(),
                    "{label}: the Write {id} of {target} did not fail (is_error {is_error:?}) and the gate never \
                     saw that call -- no host answer to its gate request and no card for it (an answer to the \
                     CLI's own prompt, or a card for it, is not the gate: answered {:?}, carded {:?}). Host \
                     answered: {:?}, cards: {:?}",
                    self.prompt_answered,
                    self.prompt_carded_calls,
                    self.host_answered(),
                    self.carded_calls
                );
            }
            if is_error == &Some(false) {
                gated_successes += 1;
            }
            seen.extend(saw);
        }
        assert!(
            !exists || gated_successes > 0,
            "{label}: {target} exists but no Write of it succeeded through the gate (writes: {writes:?}, host \
             answered: {:?}, cards: {:?}): something else wrote it, or the gate never saw the call",
            self.host_answered(),
            self.carded_calls
        );
        seen
    }

    /// For a call a bypass session ran: finds the calls of `tool` whose input satisfies `wanted`
    /// (there must be at least one), and requires of each, by its own id, that the host answered its
    /// gate request itself, that no card was drawn for it, that on the sidecar (`resolution`) that very
    /// request's resolution arrived as `allowed`, and that the call's own completion is not an error.
    /// Returns the ids. Nothing else in the turn -- another call's answer, resolution or result -- counts.
    fn assert_bypass_ran(
        &self,
        tool: &str,
        wanted: impl Fn(&serde_json::Value) -> bool,
        resolution: bool,
        label: &str,
    ) -> Vec<String> {
        let calls = self.calls_where(tool, wanted);
        assert!(
            !calls.is_empty(),
            "{label}: the model never made the {tool} call the probe asked for (calls: {:?})",
            self.calls
        );
        for (id, is_error) in &calls {
            assert_eq!(
                self.gate_saw(id),
                Some(GateSaw::HostAnswer),
                "{label}: bypass must answer the gate request of {tool} {id} itself, with no card (host answered \
                 {:?}, cards {:?})",
                self.host_answered(),
                self.carded_calls
            );
            assert!(
                !self.prompt_carded_calls.contains(id),
                "{label}: bypass drew a card for the CLI's own prompt on {tool} {id}"
            );
            assert_eq!(
                is_error,
                &Some(false),
                "{label}: {tool} {id} ran without error: {:?}",
                self.completed.get(id)
            );
            if resolution {
                let permission_id = self
                    .gate_permission_id(id)
                    .unwrap_or_else(|| panic!("{label}: no recorded answer for {tool} {id}"));
                assert_eq!(
                    self.resolved.get(permission_id).map(String::as_str),
                    Some("allowed"),
                    "{label}: the sidecar's own resolution of the request for {tool} {id} must arrive as allowed \
                     even though no card was drawn (resolutions: {:?})",
                    self.resolved
                );
            }
        }
        calls.into_iter().map(|(id, _)| id).collect()
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

/// `drive_turn`, collecting every event it saw into one `Vec` for the caller to assert on, and
/// feeding each batch to `trace` as it arrives.
fn effects_turn(
    set: &mut TabSet,
    dir: &std::path::Path,
    tab: TabId,
    prompt: &str,
    label: &str,
    trace: &mut GateTrace,
) -> Vec<serde_json::Value> {
    let mut events = Vec::new();
    drive_turn(set, dir, tab, prompt, label, |set, new| {
        trace.observe(set, tab, new);
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
    let mut trace = GateTrace::new(&dir);

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
        &mut trace,
    );
    print_tool_io(&events, "Write", "write-outside");
    assert_no_permission_requested(&events, "write-outside");
    trace.assert_bypass_ran(
        "Write",
        |input| {
            input["file_path"]
                .as_str()
                .is_some_and(|p| names_the_path(p, &dir, &write_target))
        },
        true,
        "write-outside",
    );
    assert!(write_target.exists(), "the write reached disk outside the project");
    assert_eq!(
        std::fs::read_to_string(&write_target).unwrap().trim(),
        "OUTSIDE-OK",
        "the content the prompt asked for"
    );

    let events = effects_turn(
        &mut set,
        &dir,
        tab,
        &format!(
            "Use the Bash tool to run exactly this command, unchanged: rm -rf {}",
            victim.display()
        ),
        "rm-rf-outside",
        &mut trace,
    );
    print_tool_io(&events, "Bash", "rm-rf-outside");
    assert_no_permission_requested(&events, "rm-rf-outside");
    trace.assert_bypass_ran(
        "Bash",
        |input| input["command"].as_str().map(str::trim) == Some(format!("rm -rf {}", victim.display()).as_str()),
        true,
        "rm-rf-outside",
    );
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
        &mut trace,
    );
    print_tool_io(&events, "Read", "read-outside");
    assert_no_permission_requested(&events, "read-outside");
    trace.assert_bypass_ran(
        "Read",
        |input| {
            input["file_path"]
                .as_str()
                .is_some_and(|p| names_the_path(p, &dir, &read_target))
        },
        true,
        "read-outside",
    );
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
        AgentBackend::start(
            BackendKind::Sidecar,
            &dir,
            None,
            agent::setting_sources::ProjectTrust::Trusted,
        )
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
    // Which of f1..f5 those `Read`s were of: one call per file is what the prompt asks for.
    let mut read_files: BTreeSet<i32> = BTreeSet::new();
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
                    // Only a `Read` of one of the five files the prompt names: a `Read` of anything else
                    // is not a figure of this measurement.
                    AgentDomainEvent::ToolCallStarted {
                        name,
                        tool_use_id,
                        input,
                        ..
                    } if name == "Read" => {
                        let file = (1..=5).find(|i| {
                            input["file_path"]
                                .as_str()
                                .is_some_and(|p| names_the_path(p, &dir, Path::new(&format!("f{i}.txt"))))
                        });
                        if let Some(file) = file {
                            read_files.insert(file);
                            read_started.entry(tool_use_id.clone()).or_insert(now_ms);
                        }
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

    assert!(
        read_started.len() == 5 && read_files.len() == 5,
        "the prompt asks for five Reads, one per file f1..f5; any other set is a run to repeat, not a figure \
         (reads: {read_started:?}, files: {read_files:?})"
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
    let mut trace = GateTrace::new(&dir);

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
        &mut trace,
    );
    // Printed first: the last legacy run's `rm -rf` left the directory, and nothing said why.
    print_tool_io(&events, "Write", "write-outside");
    assert_no_permission_requested(&events, "write-outside");
    trace.assert_bypass_ran(
        "Write",
        |input| {
            input["file_path"]
                .as_str()
                .is_some_and(|p| names_the_path(p, &dir, &write_target))
        },
        false,
        "write-outside",
    );
    assert!(write_target.exists(), "the write reached disk outside the project");
    assert_eq!(
        std::fs::read_to_string(&write_target).unwrap().trim(),
        "OUTSIDE-OK",
        "the content the prompt asked for"
    );

    let events = effects_turn(
        &mut set,
        &dir,
        tab,
        &format!(
            "Use the Bash tool to run exactly this command, unchanged: rm -rf {}",
            victim.display()
        ),
        "rm-rf-outside",
        &mut trace,
    );
    // Printed first: the last legacy run's `rm -rf` left the directory, and nothing said why.
    print_tool_io(&events, "Bash", "rm-rf-outside");
    assert_no_permission_requested(&events, "rm-rf-outside");
    trace.assert_bypass_ran(
        "Bash",
        |input| input["command"].as_str().map(str::trim) == Some(format!("rm -rf {}", victim.display()).as_str()),
        false,
        "rm-rf-outside",
    );
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
        &mut trace,
    );
    // Printed first: the last legacy run's `rm -rf` left the directory, and nothing said why.
    print_tool_io(&events, "Read", "read-outside");
    assert_no_permission_requested(&events, "read-outside");
    trace.assert_bypass_ran(
        "Read",
        |input| {
            input["file_path"]
                .as_str()
                .is_some_and(|p| names_the_path(p, &dir, &read_target))
        },
        false,
        "read-outside",
    );
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

/// A `Write` a subagent attempted (a sidechain `tool_use` named `Write`) and what the CLI answered it.
#[derive(Debug)]
struct SidechainWrite {
    id: String,
    agent_id: Option<String>,
    /// The `file_path` the call was given.
    file_path: Option<String>,
    /// Whether its `tool_result` was an error: `None` when no result line is there (yet), which is
    /// not the same as a success -- a successful result carries no `is_error` at all, so it reads
    /// `Some(false)`.
    is_error: Option<bool>,
    result_text: String,
}

/// Every `Write` a subagent attempted, with what the CLI answered it.
fn sidechain_writes(lines: &[(PathBuf, serde_json::Value)]) -> Vec<SidechainWrite> {
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
                writes.push(SidechainWrite {
                    id,
                    agent_id: line["agentId"].as_str().map(str::to_string),
                    file_path: block["input"]["file_path"].as_str().map(str::to_string),
                    is_error: result.as_ref().map(|r| r["is_error"].as_bool().unwrap_or(false)),
                    result_text: result
                        .map(|r| r["content"].to_string())
                        .unwrap_or_else(|| "<no tool_result>".into()),
                });
            }
        }
    }
    writes
}

/// Whether the transcript has settled: every subagent `Write` it records has its own result line. The
/// CLI writes the call and its result as separate lines on its own schedule, so judging a call
/// before its result is there would read a refusal as an unknown outcome.
fn sidechain_writes_have_results(lines: &[(PathBuf, serde_json::Value)]) -> bool {
    sidechain_writes(lines).iter().all(|w| w.is_error.is_some())
}

/// The subagent `Write`s that the gate has no card and no host answer for, split by what is known of
/// them: `(ran, unresolved)`. `ran` are calls whose result is there and is not an error: a leak.
/// `unresolved` are calls with no result line at all: nothing can be said, which is not a leak.
fn sidechain_writes_the_gate_never_saw<'a>(
    attempts: &'a [SidechainWrite],
    trace: &GateTrace,
) -> (Vec<&'a SidechainWrite>, Vec<&'a SidechainWrite>) {
    let unseen = attempts.iter().filter(|w| trace.gate_saw(&w.id).is_none());
    let ran = unseen.clone().filter(|w| w.is_error == Some(false)).collect();
    let unresolved = unseen.filter(|w| w.is_error.is_none()).collect();
    (ran, unresolved)
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
/// slip through unasked? Card + file: the gate sees subagents (bypass will answer them too). The
/// host answering the call itself (a fast-path `allow` noted on its row, or a `defer`) + file: the same
/// evidence, since the host's answer is the gate seeing the call. No card, no answer of the host +
/// no file: the CLI's `default` refused an unhooked call (spec §2.5, a functional gap, an O3 trigger --
/// recorded, not failed). No card, no answer of the host + file: a leak -- a security finding, and a
/// failure.
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
    let mut trace = GateTrace::new(&dir);
    drive_turn(
        &mut set,
        &dir,
        tab,
        "Use the Agent tool (called Task on older versions) to launch exactly one subagent whose only \
         job is to use the Write tool to create a file named sub.txt in the current directory \
         containing the word ok. Do not use the Write tool yourself, and do nothing else yourself.",
        label,
        |set, new| {
            trace.observe(set, tab, new);
            // (permission id, tool-use id, tool, input, delivered as a card rather than found pending).
            let mut asks: Vec<(String, Option<String>, String, serde_json::Value, bool)> = Vec::new();
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
                        asks.push((permission_id, tool_use_id, tool, event["input"].clone(), true));
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
                    if wanted && !asks.iter().any(|(id, ..)| id == &request.permission_id) {
                        asks.push((
                            request.permission_id.clone(),
                            request.tool_use_id.clone().filter(|t| !t.is_empty()),
                            request.tool_name.clone(),
                            request.input.clone(),
                            false,
                        ));
                    }
                }
            }
            for (permission_id, tool_use_id, tool, input, delivered) in asks {
                if !answered.insert(permission_id.clone()) {
                    continue;
                }
                let decision = if is_subagent_tool(&tool) {
                    subagent_card_answered |= delivered;
                    PermissionDecision::Allow
                } else if tool == "Write"
                    && input["file_path"]
                        .as_str()
                        .is_some_and(|path| names_the_path(path, &dir, Path::new("sub.txt")))
                {
                    // Only the file the probe is about: a card for a Write of anything else is denied.
                    write_cards.push(WriteCard {
                        permission_id: permission_id.clone(),
                        tool_use_id,
                        while_subagent_open: !open_subagent_calls.is_empty(),
                    });
                    PermissionDecision::Allow
                } else {
                    denied.push(format!("{tool} {input}"));
                    PermissionDecision::Deny {
                        reason: Some("v1-mode probe: only the subagent and its Write of sub.txt are allowed".into()),
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
    let target = Path::new("sub.txt");
    let sub_txt_exists = dir.join(target).exists();
    let provider_session_id = set
        .get(tab)
        .unwrap()
        .provider_session_id()
        .expect("a session that ran a turn has a Claude session id");
    // The transcript is the CLI's own file: give it a moment to be flushed after the turn's end. A
    // subagent's is written on its own schedule, so wait for the line of every Write of the target
    // the turn made, by whatever route the gate saw it (a card, or the host's own answer), or none:
    // judging before a line is there would find no attempt for a write the host answered and report
    // it as one the gate missed.
    let target_write_ids: Vec<String> = trace.writes_to(target).into_iter().map(|(id, _)| id).collect();
    let mut lines = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    let transcript_has = |lines: &[(PathBuf, serde_json::Value)]| {
        write_cards
            .iter()
            .filter_map(|c| c.tool_use_id.as_deref())
            .chain(target_write_ids.iter().map(String::as_str))
            .all(|id| tool_use_origin(lines, id).is_some())
    };
    while Instant::now() < deadline {
        lines = session_transcript_lines(&provider_session_id);
        if !lines.is_empty() && transcript_has(&lines) && sidechain_writes_have_results(&lines) {
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
    // The subagent's own Writes of the target, each by its tool-use id.
    let target_attempts: Vec<&SidechainWrite> = attempts
        .iter()
        .filter(|w| {
            w.file_path
                .as_deref()
                .is_some_and(|path| names_the_path(path, &dir, target))
        })
        .collect();
    println!(
        "[v1-mode] test3 subagent-write on {}: subagent_calls={subagent_calls:?} subagent_card_answered=\
         {subagent_card_answered} write_cards={write_cards:?} origins={origins:?} sub.txt_exists=\
         {sub_txt_exists} denied={denied:?} host_answered={:?} prompt_answered={:?} target_writes={:?} \
         transcript_lines={} sidechain_write_attempts={attempts:?}",
        kind.as_str(),
        trace.host_answered(),
        trace.prompt_answered,
        trace.writes_to(target),
        lines.len()
    );
    println!("[v1-mode] test3 timeline on {}: {timeline:#?}", kind.as_str());

    // The disk-side checks come first, whatever the rest says. Every Write of sub.txt that did not
    // fail outright, by whoever made it, must have been seen by the gate by its own tool-use id, and
    // a sub.txt on disk must be the work of one that succeeded through it: another call's answer, even
    // another Write's, excuses nothing.
    let finding = format!("SECURITY FINDING on {}", kind.as_str());
    trace.assert_target_writes_were_gated("sub.txt", sub_txt_exists, &finding);
    // A subagent Write of ANY path that ran, with no card and no answer of the host for its tool-use
    // id, is a leak whatever else happened in the turn. One whose result line never appeared within the
    // bound says nothing: that is a run to repeat, not a leak.
    let (ran_unasked, unresolved) = sidechain_writes_the_gate_never_saw(&attempts, &trace);
    assert!(
        ran_unasked.is_empty(),
        "{finding}: a subagent Write ran with no card ever drawn for it and no answer of the host to it: \
         {ran_unasked:?}"
    );
    assert!(
        unresolved.is_empty(),
        "INCONCLUSIVE on {}: a subagent Write the gate has no record of has no result in the transcript \
         either (the CLI's file was not complete within the wait), so whether it ran is unknown -- rerun: \
         {unresolved:?}",
        kind.as_str()
    );
    assert!(
        !subagent_calls.is_empty(),
        "INCONCLUSIVE on {}: the model never started an Agent/Task call, so no subagent path was exercised \
         -- rerun; this is not a result",
        kind.as_str()
    );

    // The gate saw a subagent's Write of sub.txt: a sidechain line with an agentId records that very
    // call, and the gate has a card or a host answer for that very tool-use id. For a card, the call
    // must also have been raised while the Agent/Task call was still open.
    let seen_by_the_gate: Vec<&&SidechainWrite> = target_attempts
        .iter()
        .filter(|w| w.agent_id.as_deref().is_some_and(|agent| !agent.is_empty()))
        .filter(|w| match trace.gate_saw(&w.id) {
            Some(GateSaw::Card) => write_cards
                .iter()
                .any(|c| c.tool_use_id.as_deref() == Some(w.id.as_str()) && c.while_subagent_open),
            Some(GateSaw::HostAnswer) => true,
            None => false,
        })
        .collect();
    if let Some(w) = seen_by_the_gate.first() {
        println!(
            "[v1-mode] test3 on {}: the gate sees a subagent's Write ({:?} for {} from subagent {:?}, \
             sub.txt_exists={sub_txt_exists})",
            kind.as_str(),
            trace.gate_saw(&w.id),
            w.id,
            w.agent_id
        );
        return;
    }
    // Nothing shows the gate seeing a subagent's Write of sub.txt.
    assert!(
        !sub_txt_exists,
        "INCONCLUSIVE on {}: sub.txt exists and the gate saw the Write that made it, but no subagent \
         Write of sub.txt is shown to have been seen by the gate -- the main agent may have written it \
         itself (sidechain attempts: {target_attempts:?}, target writes: {:?}); rerun",
        kind.as_str(),
        trace.writes_to(target)
    );
    assert!(
        !target_attempts.is_empty() || !write_cards.is_empty(),
        "INCONCLUSIVE on {}: no card, no answer of the host, no sub.txt, and the transcript shows no \
         subagent Write of sub.txt either -- the subagent never tried; rerun",
        kind.as_str()
    );
    assert!(
        write_cards.is_empty(),
        "INCONCLUSIVE on {}: a Write card for sub.txt arrived, but none is shown to be the subagent's (open \
         Agent/Task call + a sidechain transcript line with an agentId for that tool-use id) -- the main \
         agent may have written it itself; rerun, and do not record `card_seen` as the gate seeing \
         subagents (write cards: {write_cards:?}, sidechain attempts: {target_attempts:?})",
        kind.as_str()
    );
    eprintln!(
        "[v1-mode] O3/§2.5 TRIGGER on {}: the subagent's Write of sub.txt reached neither the gate nor the \
         disk -- the CLI's `default` mode refused a call the hook never saw; report this in concerns, do \
         not loosen anything. What the CLI answered: {:?}",
        kind.as_str(),
        target_attempts
            .iter()
            .map(|w| (&w.id, w.is_error, &w.result_text))
            .collect::<Vec<_>>()
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
/// Verdandi b3aa188 routes that ask to the host (`provider_permission_prompts`), Eitri answers it.
///
/// - **Bypass** (O3 ruling 4): both writes SUCCEED with no card -- the file is on disk, the tool
///   result is not an error, nothing reached the panel as a request.
/// - **Auto** (ruling 5): what happens to a `Write` to `.git/probe2` depends on who answers the gate.
///   On a sidecar whose CLI runs its own `auto` the host defers: no card is drawn, the gate request
///   resolves `deferred`, and the CLI's classifier decides whether the write runs (a measurement,
///   printed, not asserted: nothing here promises the classifier refuses a protected path). Where the
///   host's own policy answers instead, the gate draws exactly ONE card, the human approves it
///   (`TabSet::answer_card`, the panel's own route) and the CLI's own prompt for the same call is then
///   answered without a second card; at least one allowed resolution names a permission id that was
///   never delivered -- that is the CLI's own prompt, answered by the host, so the mechanism really
///   ran. Either way the gate saw the call: no card and no answer of the host to it would be a write
///   that bypassed the gate.
///
/// Needs a sidecar advertising `provider_permission_prompts` (b3aa188 or later):
/// `EITRI_SIDECAR_BINARY=<that artifact>`.
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
        let mut requested = 0usize;
        let mut bypass_trace = GateTrace::new(&dir);
        drive_turn(&mut set, &dir, tab, &prompt, rel, |set, new| {
            bypass_trace.observe(set, tab, new);
            requested += new.iter().filter(|e| e["type"] == "permission_requested").count();
        });
        let ran = target.exists();
        println!(
            "[v1-mode] test4 bypass write {rel}: ran={ran} writes={:?} results={:?} requests_delivered={requested} \
             host_answered={:?} prompt_answered={:?} prompt_notes={:?}",
            bypass_trace.writes_to(Path::new(rel)),
            bypass_trace.completed,
            bypass_trace.host_answered(),
            bypass_trace.prompt_answered,
            bypass_trace.prompt_notes
        );
        // Bypass answers a CLI prompt only when it gives a reason: one that explains nothing could be the
        // user's own ask rule and is a card, so any prompt answered here was labelled as a plain ask,
        // never as "maybe your ask rule".
        for (call, note) in &bypass_trace.prompt_notes {
            assert!(
                note.starts_with("Claude Code asked") && !note.contains("maybe your ask rule"),
                "{rel}: bypass answered the CLI's own prompt for {call} though it gave no reason: {note:?}"
            );
        }
        assert_eq!(
            requested, 0,
            "{rel}: bypass draws no card, the CLI's own prompt included"
        );
        // The Write of this file, by its own tool-use id: the host answered its gate request (and the
        // sidecar's own resolution of that request arrived as allowed), no card was drawn for it, and
        // the call's own result is not an error -- as a real bypassPermissions session would run it.
        bypass_trace.assert_bypass_ran(
            "Write",
            |input| {
                input["file_path"]
                    .as_str()
                    .is_some_and(|path| names_the_path(path, &dir, Path::new(rel)))
            },
            true,
            rel,
        );
        assert!(
            ran,
            "O3: bypass must run a Write to {rel} as a real bypassPermissions session does; results {:?}",
            bypass_trace.completed
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap().trim(),
            "PROBE",
            "{rel}: the content the prompt asked for"
        );
        // And the file on disk is the work of a Write the gate saw.
        bypass_trace.assert_target_writes_were_gated(rel, ran, rel);
    }

    // Auto, same session: leaving bypass is immediate (D6).
    match set.cycle_mode(tab).unwrap() {
        ModeCycle::Changed(SessionModeChoice::Auto) => {}
        other => panic!("leaving bypass moves at once (D6), got {other:?}"),
    }
    let rel = ".git/probe2";
    let target = dir.join(rel);
    // (permission id, tool, tool-use id, whether it is the card for a Write of the target).
    let mut cards: Vec<(String, String, Option<String>, bool)> = Vec::new();
    let mut trace = GateTrace::new(&dir);
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
            trace.observe(set, tab, new);
            for event in new {
                if event["type"] == "permission_requested" {
                    let id = event["permission_id"].as_str().unwrap().to_string();
                    let tool = event["tool_name"].as_str().unwrap_or_default().to_string();
                    let tool_use_id = event["tool_use_id"]
                        .as_str()
                        .filter(|t| !t.is_empty())
                        .map(str::to_string);
                    let for_the_target = tool == "Write"
                        && event["input"]["file_path"]
                            .as_str()
                            .is_some_and(|path| names_the_path(path, &dir, Path::new(rel)));
                    cards.push((id.clone(), tool, tool_use_id, for_the_target));
                    // The human's Approve for the card of the Write of the target, through the panel's
                    // own route; a card for anything else is not this probe's to approve.
                    let decision = if for_the_target {
                        PermissionDecision::Allow
                    } else {
                        PermissionDecision::Deny {
                            reason: Some("v1-mode probe: only the Write of the target is approved".into()),
                        }
                    };
                    set.answer_card(tab, &id, decision).map_err(|e| e.message).unwrap();
                }
            }
        },
    );
    println!(
        "[v1-mode] test4 auto write {rel}: ran={} cards={cards:?} writes={:?} results={:?} host_answered={:?} \
         prompt_answered={:?}",
        target.exists(),
        trace.writes_to(Path::new(rel)),
        trace.completed,
        trace.host_answered(),
        trace.prompt_answered
    );
    // Each write of the target, judged by its own call, and the disk first: an answer to some other
    // Write is no evidence, and a file on disk must be the work of a write the gate saw.
    let gate_saw = trace.assert_target_writes_were_gated(rel, target.exists(), rel);
    let target_cards: Vec<&(String, String, Option<String>, bool)> = cards.iter().filter(|c| c.3).collect();
    if target_cards.is_empty() {
        // The host answered the gate request of the Write of the target itself, so the write is the
        // CLI's to allow or refuse.
        assert!(
            gate_saw.contains(&GateSaw::HostAnswer),
            "{rel}: no card for the Write of {rel} and no answer of the host to its gate request -- a write \
             that ran, or never started, without the gate: host answered {:?}, calls {:?}, ran={}",
            trace.host_answered(),
            trace.calls,
            target.exists()
        );
        println!(
            "[v1-mode] test4 auto write {rel}: the host answered the gate itself ({:?}) and left the \
             decision to the CLI: ran={}",
            trace.host_answered(),
            target.exists()
        );
        return;
    }
    assert_eq!(
        target_cards.len(),
        1,
        "exactly one card for the Write of {rel} -- the CLI's own prompt that follows the human's approval \
         is answered without a second one: {cards:?}"
    );
    let card_call = target_cards[0]
        .2
        .as_deref()
        .unwrap_or_else(|| panic!("{rel}: the card names no tool call: {cards:?}"));
    assert_eq!(
        trace.gate_saw(card_call),
        Some(GateSaw::Card),
        "{rel}: the card is the gate's own request for the Write {card_call}: {cards:?}"
    );
    assert_eq!(
        trace.completed.get(card_call).map(|(is_error, _)| *is_error),
        Some(Some(false)),
        "{rel}: the approved Write {card_call} ran without error: {:?}",
        trace.completed.get(card_call)
    );
    assert!(
        target.exists(),
        "the approved write reached disk: {:?}",
        trace.completed.get(card_call)
    );
    assert!(
        trace.prompt_answered.contains(card_call),
        "the CLI's own prompt for the same call {card_call}, answered by the host without a card, must show \
         the mechanism ran: cards={cards:?} prompt_answered={:?}",
        trace.prompt_answered
    );
}

/// Test 5 (D12): a project's own `permissions.defaultMode` must never leave a session ungated. Each of
/// the four combinations must land in exactly one of three branches: `Gated` (a card arrives; deny it;
/// nothing written), `Tripped` (D12 closes the session; nothing written) or `AnsweredByHost` (no card,
/// because the host answered the gate request itself: a `defer` to the CLI's own auto mode, or an
/// `allow` from its edit fast path). `inside.txt` existing without the gate having seen the call --
/// a card answered `Allow`, or an answer of the host -- is never acceptable in any branch, and is
/// asserted as such regardless of which one this run took. The CLI must also report a mode the host
/// accepts (`auto` or `default`, never what the project asked for). On legacy the explicit
/// `--permission-mode default` must always win: the branch there must not be `Tripped`.
enum DefaultModeBranch {
    Gated,
    AnsweredByHost,
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
    let backend = AgentBackend::start(kind, &dir, None, agent::setting_sources::ProjectTrust::Trusted)
        .map_err(|e| e.message)
        .expect("a session starts; is this running under a test-account wrapper?");
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    let mut trace = GateTrace::new(&dir);
    // The tool-use id of the card for the Write of inside.txt that this probe denied, if one came.
    let mut denied_call: Option<String> = None;

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
            let mut started = false;
            let deadline = Instant::now() + Duration::from_secs(240);
            let mut found = loop {
                let out = set.pump(&dir, true);
                if !out.tripped.is_empty() {
                    for mut t in out.tripped {
                        t.backend.shutdown();
                    }
                    break DefaultModeBranch::Tripped;
                }
                let events = events_from_payload(out.active_payload.as_deref());
                trace.observe(&set, tab, &events);
                for event in &events {
                    if event["type"] != "permission_requested" {
                        continue;
                    }
                    let id = event["permission_id"].as_str().unwrap_or_default().to_string();
                    let for_the_target = event["tool_name"] == "Write"
                        && event["input"]["file_path"]
                            .as_str()
                            .is_some_and(|path| names_the_path(path, &dir, Path::new("inside.txt")));
                    if for_the_target && permission_id.is_none() {
                        denied_call = event["tool_use_id"]
                            .as_str()
                            .filter(|t| !t.is_empty())
                            .map(str::to_string);
                        permission_id = Some(id);
                    } else {
                        // A card for anything else is not what this probe is about: denied at once, so
                        // it neither runs nor holds the turn open, and it makes no branch.
                        println!("[v1-mode] test5 {label}: denying a card that is not for inside.txt: {event}");
                        let _ = set.get_mut(tab).unwrap().live_mut().unwrap().respond_permission(
                            &id,
                            PermissionDecision::Deny {
                                reason: Some("v1-mode D12 probe: not the call under test".into()),
                            },
                        );
                    }
                }
                if permission_id.is_some() {
                    break DefaultModeBranch::Gated;
                }
                // No card: the turn may run to its end with the host answering the gate itself. That
                // is only the gate seeing the Write if the host's answer was to a Write.
                let running = set.get(tab).unwrap().turn_running();
                started |= running;
                if started && !running {
                    let inside = dir.join("inside.txt").exists();
                    // By the target call's own id: an answer to an unrelated Write says nothing about a
                    // write of `inside.txt` that never reached the gate.
                    let gate_saw = trace.assert_target_writes_were_gated("inside.txt", inside, label);
                    assert!(
                        gate_saw.contains(&GateSaw::HostAnswer),
                        "{label}: the turn ended with no card, no trip and no answer of the host to a Write \
                         of inside.txt (inside.txt exists: {inside}; a Write that ran this way bypassed the \
                         gate, one that never started measured nothing). Host answered: {:?}, calls: {:?}",
                        trace.host_answered(),
                        trace.calls
                    );
                    break DefaultModeBranch::AnsweredByHost;
                }
                assert!(
                    Instant::now() < deadline,
                    "{label}: neither a card, a trip nor the end of the turn arrived in time"
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
                    let events = events_from_payload(out.active_payload.as_deref());
                    trace.observe(&set, tab, &events);
                    // Whatever else the model asks while the turn winds down is denied as well.
                    for event in events.iter().filter(|e| e["type"] == "permission_requested") {
                        if let Some(id) = event["permission_id"].as_str() {
                            let _ = set.get_mut(tab).unwrap().live_mut().unwrap().respond_permission(
                                id,
                                PermissionDecision::Deny {
                                    reason: Some("v1-mode D12 probe: not the call under test".into()),
                                },
                            );
                        }
                    }
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
        DefaultModeBranch::AnsweredByHost => "answered-by-host",
        DefaultModeBranch::Tripped => "tripped",
    };
    println!(
        "[v1-mode] test5 {label} (defaultMode={default_mode}, backend={}): branch={branch_name}",
        kind.as_str()
    );

    // A branch that keeps its session means no UngatedCliMode was reported: check that too, including
    // a report the ingestion folded that no pump has turned into a trip, and that the mode the CLI
    // does report is one the host accepts. Legacy passes `--permission-mode default` explicitly, so
    // it can only ever report `default` (a sidecar that offers the CLI's own auto mode reports `auto`).
    if !matches!(branch, DefaultModeBranch::Tripped) {
        let state = set.get(tab).unwrap();
        let backend = state.live().unwrap_or_else(|| {
            panic!(
                "{label}: a tab that was not tripped keeps its session, but it is {:?}",
                state.wire_state()
            )
        });
        let ungated = backend.projection().ungated_cli_mode.clone();
        assert!(
            ungated.is_none(),
            "{label}: the CLI reported an ungated mode ({ungated:?}) on a session that was not tripped"
        );
        let reported = backend.projection().cli_mode_reported.clone();
        println!("[v1-mode] test5 {label}: the CLI reported permission mode {reported:?}");
        let accepted: &[&str] = if kind == BackendKind::Legacy {
            &["default"]
        } else {
            &["auto", "default"]
        };
        assert!(
            reported.as_deref().is_none_or(|mode| accepted.contains(&mode)),
            "{label}: the CLI reported {reported:?}, not one of {accepted:?}: the project's defaultMode \
             {default_mode} reached the session"
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

    // What became of inside.txt, judged against the calls that wrote it, each by its own tool-use id and
    // before anything else is concluded. Gated: the card for the Write was denied, so that call must not
    // have succeeded, and any other write of the file that did must have been seen by the gate.
    // AnsweredByHost: every write that did not fail outright was seen by the gate, and the file on disk
    // is the work of one that was. Tripped: the session is closed, nothing may have been written.
    let inside = dir.join("inside.txt").exists();
    match branch {
        DefaultModeBranch::Gated => {
            if let Some(id) = &denied_call {
                assert_ne!(
                    trace.completed.get(id).map(|(is_error, _)| *is_error),
                    Some(Some(false)),
                    "{label}: the Write {id} whose card was denied succeeded all the same: {:?}",
                    trace.completed.get(id)
                );
            }
            trace.assert_target_writes_were_gated("inside.txt", inside, label);
        }
        DefaultModeBranch::AnsweredByHost => {
            trace.assert_target_writes_were_gated("inside.txt", inside, label);
            println!(
                "[v1-mode] test5 {label}: the host answered the Write itself ({:?}); inside.txt exists: {inside}",
                trace.host_answered()
            );
        }
        DefaultModeBranch::Tripped => {
            assert!(
                !inside,
                "{label}: never acceptable -- inside.txt must not exist in a session the CLI ran ungated"
            );
        }
    }

    if kind == BackendKind::Legacy {
        assert!(
            !matches!(branch, DefaultModeBranch::Tripped),
            "{label}: D4 says the explicit --permission-mode default must win over the project's own \
             defaultMode on legacy, so the CLI must not report an ungated mode and trip"
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

/// Whether a tool call's input is for `file` under `dir`: a `file_path` that names it, or a shell
/// command that mentions it.
fn input_is_for(input: &serde_json::Value, dir: &Path, file: &str) -> bool {
    input["file_path"]
        .as_str()
        .is_some_and(|path| names_the_path(path, dir, Path::new(file)))
        || input["command"].as_str().is_some_and(|command| command.contains(file))
}

/// A card waiting in a tab's session for the call that names a file.
struct WaitingCard {
    permission_id: String,
    tool_use_id: Option<String>,
    prompt: Option<agent::ProviderPrompt>,
}

/// Pumps until a card for a call that names `file` has been delivered to the panel, and returns it, or
/// panics saying which one never came. A card for any other call is not it.
///
/// **Delivered, not merely pending.** On a session whose CLI runs its own `auto` the host defers the
/// gate's request at once, and that request sits in the projection's pending set until the sidecar's
/// resolution of it folds, tens of milliseconds later. Reading the pending set would sometimes return
/// that request -- one nobody is ever asked, with no provider prompt -- instead of the CLI's own prompt
/// that follows it. So the card is the one the pump itself delivered as a `permission_requested`
/// event, and one the host has answered is skipped. Its fields are then read from the projection,
/// copied out so nothing is asserted while its lock is held: a panic then would poison it and turn the
/// failure into an abort during teardown.
fn wait_for_the_card_for(set: &mut TabSet, dir: &Path, tab: TabId, file: &str, what: &str) -> WaitingCard {
    let deadline = Instant::now() + Duration::from_secs(120);
    // Every card id delivered so far, over all pumps: a card delivered in one pump is looked up in a
    // later one if the projection did not hold it yet, and one delivered inside a snapshot (a resync
    // replaces the events) counts like one delivered as an event.
    let mut delivered: BTreeSet<String> = BTreeSet::new();
    loop {
        let out = set.pump(dir, true);
        delivered.extend(
            events_from_payload(out.active_payload.as_deref())
                .iter()
                .filter(|event| event["type"] == "permission_requested" && input_is_for(&event["input"], dir, file))
                .filter_map(|event| event["permission_id"].as_str().map(str::to_string)),
        );
        if let Some(payload) = out.active_payload.as_deref() {
            let value: serde_json::Value = serde_json::from_str(payload).unwrap();
            if value["kind"] == "snapshot" {
                delivered.extend(
                    value["state"]["pendingPermissions"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|card| input_is_for(&card["input"], dir, file))
                        .filter_map(|card| card["permissionId"].as_str().map(str::to_string)),
                );
            }
        }
        let tab_ref = set.get(tab).unwrap();
        let host_answered: BTreeSet<&str> = tab_ref
            .gate_answers()
            .iter()
            .map(|answer| answer.permission_id.as_str())
            .collect();
        let found = delivered
            .iter()
            .filter(|id| !host_answered.contains(id.as_str()))
            .find_map(|id| {
                let backend = tab_ref.live().unwrap();
                let projection = backend.projection();
                projection.pending_permissions.get(id).map(|request| WaitingCard {
                    permission_id: request.permission_id.clone(),
                    tool_use_id: request.tool_use_id.clone().filter(|t| !t.is_empty()),
                    prompt: request.provider_prompt.clone(),
                })
            });
        if let Some(card) = found {
            return card;
        }
        assert!(Instant::now() < deadline, "{what}: the card for {file} never arrived");
        std::thread::sleep(Duration::from_millis(33));
    }
}

/// How the tab's session recorded the result of the call `tool_use_id`: `Some(is_error)` once it
/// completed, `None` while it has not (or never started).
fn call_result_is_error(set: &TabSet, tab: TabId, tool_use_id: &str) -> Option<bool> {
    set.get(tab)
        .unwrap()
        .live()
        .unwrap()
        .projection()
        .tool_calls
        .iter()
        .find(|call| call.tool_use_id == tool_use_id)
        .and_then(|call| call.result.as_ref().map(|result| result.is_error))
}

/// Pumps until the tab's turn has ended, denying every card that is drawn on the way: after a refusal
/// the model often tries the same call again, and an unanswered second card would hold the turn open
/// until the deadline. Returns how many cards it denied.
fn deny_cards_until_the_turn_ends(set: &mut TabSet, dir: &std::path::Path, tab: TabId, what: &str) -> usize {
    let mut denied = 0usize;
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let out = set.pump(dir, true);
        for event in events_from_payload(out.active_payload.as_deref()) {
            if event["type"] == "permission_requested" {
                if let Some(id) = event["permission_id"].as_str() {
                    denied += 1;
                    let _ = set.get_mut(tab).unwrap().live_mut().unwrap().respond_permission(
                        id,
                        PermissionDecision::Deny {
                            reason: Some("v1-mode probe".into()),
                        },
                    );
                }
            }
        }
        if !set.get(tab).unwrap().turn_running() {
            return denied;
        }
        assert!(Instant::now() < deadline, "{what}: the turn never ended");
        std::thread::sleep(Duration::from_millis(33));
    }
}

/// What test 6a does for one spelling of the ask rule: entering bypass leaves a card the user's own
/// `permissions.ask` rule forced exactly where it was. `cycle_mode` lists nothing to approve, says the
/// card stays, `confirm_bypass` approves nothing and the write does not happen until the human answers.
/// Whether the CLI names the rule in its prompt (`matched_ask_rule`) is printed, not asserted: a bare
/// tool-name rule does on CLI 2.1.288 and a content-scoped one does not, and either way the prompt must
/// be the human's.
fn bypass_leaves_the_ask_rule_card_waiting(label: &str, ask_rule: &str) {
    let dir = project(label);
    std::fs::create_dir_all(dir.join(".claude")).unwrap();
    std::fs::write(
        dir.join(".claude/settings.json"),
        serde_json::json!({ "permissions": { "ask": [ask_rule] } }).to_string(),
    )
    .unwrap();
    // Trusted (`auto_tab`), so the project's ask rule is loaded.
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
    // The card for the Write of first.txt itself: a card for any other call says nothing here.
    let card = wait_for_the_card_for(&mut set, &dir, tab, "first.txt", "the ask-rule card");
    let card_id = card.permission_id.clone();
    let prompt = card
        .prompt
        .unwrap_or_else(|| panic!("the card is the CLI's own prompt, not the host's policy"));
    println!(
        "[v1-mode] test6a {ask_rule}: prompt reason={:?} blocked_path={:?} matched_ask_rule={:?} label={:?}",
        prompt.reason,
        prompt.blocked_path,
        prompt.matched_ask_rule,
        prompt.label()
    );
    assert!(
        prompt.needs_a_human(),
        "the project's ask rule {ask_rule} forced this prompt, so only a human answers it: {prompt:?}"
    );

    let plan = match set.cycle_mode(tab).unwrap() {
        ModeCycle::Confirm(plan) => plan,
        other => panic!("entering bypass always asks first (D2), got {other:?}"),
    };
    assert!(
        !plan.approve.contains(&card_id),
        "an ask-rule card is never listed for approval: {:?}",
        plan.approve
    );
    assert!(
        plan.lines.len() >= 2,
        "the prompt says that the card stays waiting: {:?}",
        plan.lines
    );
    match set.confirm_bypass(plan.scope, plan.nonce).unwrap() {
        ConfirmOutcome::Entered { approved, .. } => {
            assert_eq!(approved, plan.approve.len(), "only what the plan listed was approved")
        }
        other => panic!("expected Entered, got {other:?}"),
    }
    assert_eq!(set.get(tab).unwrap().mode(), SessionModeChoice::Bypass);

    // Give the CLI and the pump a moment to do whatever approving would have caused.
    for _ in 0..30 {
        set.pump(&dir, true);
        std::thread::sleep(Duration::from_millis(33));
    }
    assert!(
        set.get(tab)
            .unwrap()
            .live()
            .unwrap()
            .projection()
            .pending_permissions
            .contains_key(&card_id),
        "the ask-rule card is still waiting after bypass was entered"
    );
    assert!(
        !dir.join("first.txt").exists(),
        "entering bypass must not have approved the ask-rule card"
    );
    if let Some(call) = &card.tool_use_id {
        assert_ne!(
            call_result_is_error(&set, tab, call),
            Some(false),
            "the Write {call} behind the ask-rule card must not have run"
        );
    }

    // The human's own answer is what ends it.
    let _ = set.get_mut(tab).unwrap().live_mut().unwrap().respond_permission(
        &card_id,
        PermissionDecision::Deny {
            reason: Some("v1-mode probe".into()),
        },
    );
    deny_cards_until_the_turn_ends(&mut set, &dir, tab, "after the denial");
    assert!(!dir.join("first.txt").exists(), "denied: nothing written");
}

/// Test 6a, a bare tool-name rule: the spelling on CLI 2.1.288 whose prompt names the rule
/// (`matched_ask_rule`), which makes it the human's in every mode. The opposite half, a card that
/// entering bypass does approve, is test 6b.
#[test]
#[ignore = "real Claude; run under a test-account wrapper, see the module doc"]
fn entering_bypass_leaves_an_ask_rule_card_waiting() {
    bypass_leaves_the_ask_rule_card_waiting("mid-turn-bypass-ask-rule", "Write");
}

/// Test 6a, a content-scoped rule: on CLI 2.1.288 its prompt arrives with no reason, no matched rule
/// and no blocked path, because the CLI leaves `decision_reason` out for a rule it matched itself and
/// the Agent SDK drops the reason type. A prompt that explains nothing is a card in every mode, so it
/// stays waiting here just as the bare rule's does. (Entering bypass used to approve it, and the write
/// ran, in 4 of 4 runs.)
#[test]
#[ignore = "real Claude; run under a test-account wrapper, see the module doc"]
fn entering_bypass_leaves_a_content_scoped_ask_rule_card_waiting() {
    bypass_leaves_the_ask_rule_card_waiting("mid-turn-bypass-ask-rule-scoped", "Edit(first.txt)");
}

/// Test 6a, a content-scoped rule on a tab that is ALREADY in bypass: there is no confirm to leave the
/// card out of, so this is the path that used to answer the CLI's prompt `allow` with no card at all.
/// The call must still reach a card, and nothing is written until the human answers it.
#[test]
#[ignore = "real Claude; run under a test-account wrapper, see the module doc"]
fn a_bypass_tab_cards_a_content_scoped_ask_rule_prompt() {
    let dir = project("bypass-tab-ask-rule-scoped");
    std::fs::create_dir_all(dir.join(".claude")).unwrap();
    std::fs::write(
        dir.join(".claude/settings.json"),
        serde_json::json!({ "permissions": { "ask": ["Edit(first.txt)"] } }).to_string(),
    )
    .unwrap();
    let (mut set, tab) = bypass_tab(BackendKind::Sidecar, &dir);
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
    let card = wait_for_the_card_for(&mut set, &dir, tab, "first.txt", "the ask-rule card in a bypass tab");
    let prompt = card
        .prompt
        .clone()
        .unwrap_or_else(|| panic!("the card is the CLI's own prompt, not the host's policy"));
    assert!(prompt.needs_a_human(), "{prompt:?}");
    for _ in 0..30 {
        set.pump(&dir, true);
        std::thread::sleep(Duration::from_millis(33));
    }
    assert!(
        !dir.join("first.txt").exists(),
        "nothing is written before the human answers"
    );
    let _ = set.get_mut(tab).unwrap().live_mut().unwrap().respond_permission(
        &card.permission_id,
        PermissionDecision::Deny {
            reason: Some("v1-mode probe".into()),
        },
    );
    deny_cards_until_the_turn_ends(&mut set, &dir, tab, "after the denial");
    assert!(!dir.join("first.txt").exists(), "denied: nothing written");
}

/// What test 6b does on either backend: a card is waiting, entering bypass lists exactly that card and
/// approves it, the write lands, leaving bypass is immediate (D6), and a second such call cards again
/// on the now-auto tab (denied, to leave nothing behind). `ask_rules` go into the project's settings
/// (trusted, so loaded) when given.
///
/// The waiting card must be one bypass may approve: the host's own policy raised it, or the CLI did
/// and said why (`needs_a_human` false). The precondition is asserted, so a CLI that changes what it
/// reports fails here with that said, not as a plan that lists nothing. Only legacy has such a card to
/// offer in a test: on a sidecar whose CLI runs `auto` the host defers every gate request, so no card
/// of the host's exists, and the CLI's own sensitive-file prompt, the one that carries a reason, does
/// not arise there (test 4 prints which); the one a CLI in `default` raises is checked by test 4 on a
/// sidecar that does not offer the CLI's auto mode.
fn bypass_approves_the_waiting_card(
    kind: BackendKind,
    label: &str,
    ask_rules: &[&str],
    first_prompt: &str,
    second_prompt: &str,
) {
    let dir = project(label);
    if !ask_rules.is_empty() {
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::write(
            dir.join(".claude/settings.json"),
            serde_json::json!({ "permissions": { "ask": ask_rules } }).to_string(),
        )
        .unwrap();
    }
    // Trusted (`auto_tab`), so the project's ask rules are loaded.
    let (mut set, tab) = auto_tab(kind, &dir);

    set.get_mut(tab)
        .unwrap()
        .live_mut()
        .unwrap()
        .send_turn(first_prompt, "first-write")
        .map_err(|e| e.message)
        .unwrap();
    // The card for the first call itself: a card for any other call says nothing here.
    let card = wait_for_the_card_for(&mut set, &dir, tab, "first.txt", "the first card");
    let card_call = card
        .tool_use_id
        .clone()
        .unwrap_or_else(|| panic!("the card names no tool call, so its result cannot be followed"));
    assert!(
        !card.prompt.as_ref().is_some_and(|prompt| prompt.needs_a_human()),
        "this fixture must draw a card that bypass may approve, but a human-only one is waiting: {:?}",
        card.prompt
    );

    let plan = match set.cycle_mode(tab).unwrap() {
        ModeCycle::Confirm(plan) => plan,
        other => panic!("expected Confirm with the waiting card listed, got {other:?}"),
    };
    assert!(
        plan.approve.contains(&card.permission_id),
        "the plan lists the waiting card {}: {:?}",
        card.permission_id,
        plan.approve
    );

    match set.confirm_bypass(plan.scope, plan.nonce).unwrap() {
        ConfirmOutcome::Entered { approved, .. } => {
            assert_eq!(approved, plan.approve.len(), "every listed card was approved")
        }
        other => panic!("expected Entered, got {other:?}"),
    }

    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        set.pump(&dir, true);
        if !set.get(tab).unwrap().turn_running() {
            break;
        }
        assert!(Instant::now() < deadline, "the approved call never finished");
        std::thread::sleep(Duration::from_millis(33));
    }
    // The call behind that card, by its own id, completed without error, and its effect is on disk.
    assert_eq!(
        call_result_is_error(&set, tab, &card_call),
        Some(false),
        "the call {card_call} behind the approved card ran without error"
    );
    assert!(
        dir.join("first.txt").exists(),
        "the card the confirm listed was really approved"
    );

    match set.cycle_mode(tab).unwrap() {
        ModeCycle::Changed(SessionModeChoice::Auto) => {}
        other => panic!("leaving bypass moves at once (D6), got {other:?}"),
    }
    assert_eq!(set.get(tab).unwrap().mode(), SessionModeChoice::Auto);

    // A plain `effects_turn` would deadlock here: with the tab back in `auto` this call cards and the
    // turn cannot end until something answers it, so each card has to be denied AS it arrives, inside
    // `drive_turn`'s own loop, not after it returns. Every card of the turn is denied, not only the
    // first: after a refusal the model often tries again.
    let mut cards_seen = 0usize;
    drive_turn(&mut set, &dir, tab, second_prompt, "second-write", |set, new| {
        for event in new {
            if event["type"] == "permission_requested" {
                if let Some(id) = event["permission_id"].as_str() {
                    // Counted only for the second call itself; every card is denied all the same.
                    if input_is_for(&event["input"], &dir, "second.txt") {
                        cards_seen += 1;
                    }
                    let _ = set.get_mut(tab).unwrap().live_mut().unwrap().respond_permission(
                        id,
                        PermissionDecision::Deny {
                            reason: Some("v1-mode probe".into()),
                        },
                    );
                }
            }
        }
    });
    assert!(
        cards_seen >= 1,
        "a call for second.txt cards again now that the tab is back in auto (D6)"
    );
    assert!(!dir.join("second.txt").exists(), "denied: nothing written");
}

/// Test 6b, legacy: Eitri's own policy answers the gate there, and a `Bash` command with a redirect
/// cards (the policy has no shell parser), so no ask rule is involved at all.
#[cfg(feature = "legacy-backend")]
#[test]
#[ignore = "real Claude; run under a test-account wrapper, see the module doc"]
fn entering_bypass_mid_turn_approves_the_waiting_card_legacy() {
    bypass_approves_the_waiting_card(
        BackendKind::Legacy,
        "mid-turn-bypass-legacy",
        &[],
        "Use the Bash tool to run exactly this command: echo ok > first.txt",
        "Use the Bash tool to run exactly this command: echo ok > second.txt",
    );
}

/// The evidence rules, on synthetic events: these run in a plain `cargo test`, no Claude involved. Each
/// case is a way a probe used to pass on evidence that belonged to some other call.
#[cfg(test)]
mod evidence {
    use super::*;
    use eitri_core::agent_backend::GateAnswer;
    use serde_json::json;

    fn started(id: &str, tool: &str, input: serde_json::Value) -> serde_json::Value {
        json!({"type": "tool_call_started", "tool_use_id": id, "name": tool, "input": input})
    }

    fn completed(id: &str, is_error: bool) -> serde_json::Value {
        json!({"type": "tool_call_completed", "tool_use_id": id, "is_error": is_error, "content": "x"})
    }

    fn answer(origin: GateOrigin, permission_id: &str, call: &str, tool: &str) -> GateAnswer {
        GateAnswer {
            origin,
            permission_id: permission_id.into(),
            tool_use_id: Some(call.into()),
            tool_name: tool.into(),
            deferred: true,
        }
    }

    fn write_of(path: &str) -> serde_json::Value {
        json!({"file_path": path, "content": "ok"})
    }

    #[test]
    fn a_gated_write_of_the_target_passes() {
        let mut trace = GateTrace::new(Path::new("/p"));
        trace.record_answers(&[answer(GateOrigin::GateRequest, "perm-1", "w1", "Write")]);
        trace.observe_events(&[started("w1", "Write", write_of("/p/sub.txt")), completed("w1", false)]);
        let saw = trace.assert_target_writes_were_gated("sub.txt", true, "t");
        assert_eq!(saw, vec![GateSaw::HostAnswer]);
    }

    #[test]
    #[should_panic(expected = "never saw that call")]
    fn an_answer_to_another_write_does_not_excuse_an_ungated_write_of_the_target() {
        let mut trace = GateTrace::new(Path::new("/p"));
        trace.record_answers(&[answer(GateOrigin::GateRequest, "perm-1", "w1", "Write")]);
        trace.observe_events(&[
            started("w1", "Write", write_of("/p/other.txt")),
            completed("w1", true),
            started("w2", "Write", write_of("/p/sub.txt")),
            completed("w2", false),
        ]);
        trace.assert_target_writes_were_gated("sub.txt", true, "t");
    }

    #[test]
    #[should_panic(expected = "never saw that call")]
    fn an_answer_to_the_clis_own_prompt_is_not_the_gate() {
        let mut trace = GateTrace::new(Path::new("/p"));
        trace.record_answers(&[answer(GateOrigin::ProviderPrompt, "perm-1", "w1", "Write")]);
        trace.observe_events(&[
            started("w1", "Write", write_of("/p/.git/probe")),
            json!({"type": "permission_requested", "tool_use_id": "w1", "permission_id": "perm-1",
                   "provider_prompt": {"description": "x"}}),
            completed("w1", false),
        ]);
        trace.assert_target_writes_were_gated(".git/probe", true, "t");
    }

    #[test]
    #[should_panic(expected = "never saw that call")]
    fn a_write_that_never_completed_is_not_excused() {
        let mut trace = GateTrace::new(Path::new("/p"));
        trace.observe_events(&[started("w1", "Write", write_of("/p/inside.txt"))]);
        trace.assert_target_writes_were_gated("inside.txt", false, "t");
    }

    #[test]
    #[should_panic(expected = "exists but no Write of it succeeded through the gate")]
    fn a_target_on_disk_needs_a_gated_write() {
        let mut trace = GateTrace::new(Path::new("/p"));
        trace.record_answers(&[answer(GateOrigin::GateRequest, "perm-1", "w1", "Write")]);
        trace.observe_events(&[started("w1", "Write", write_of("/p/other.txt")), completed("w1", false)]);
        trace.assert_target_writes_were_gated("inside.txt", true, "t");
    }

    #[test]
    #[should_panic(expected = "must answer the gate request")]
    fn bypass_needs_the_host_answer_of_that_very_call() {
        let mut trace = GateTrace::new(Path::new("/p"));
        trace.record_answers(&[answer(GateOrigin::GateRequest, "perm-1", "other", "Bash")]);
        trace.observe_events(&[started("w1", "Write", write_of("/o/w.txt")), completed("w1", false)]);
        trace.assert_bypass_ran("Write", |i| i["file_path"] == "/o/w.txt", false, "t");
    }

    #[test]
    #[should_panic(expected = "resolution of the request")]
    fn bypass_needs_the_resolution_of_that_very_request() {
        let mut trace = GateTrace::new(Path::new("/p"));
        trace.record_answers(&[
            answer(GateOrigin::GateRequest, "perm-1", "w1", "Write"),
            answer(GateOrigin::GateRequest, "perm-2", "other", "ToolSearch"),
        ]);
        trace.observe_events(&[
            started("w1", "Write", write_of("/o/w.txt")),
            completed("w1", false),
            json!({"type": "permission_resolved", "permission_id": "perm-2", "outcome": "allowed"}),
        ]);
        trace.assert_bypass_ran("Write", |i| i["file_path"] == "/o/w.txt", true, "t");
    }

    fn sidechain_line(block: serde_json::Value) -> (PathBuf, serde_json::Value) {
        (
            PathBuf::from("agent.jsonl"),
            json!({"isSidechain": true, "agentId": "a1", "message": {"content": [block]}}),
        )
    }

    fn write_use(id: &str, path: &str) -> (PathBuf, serde_json::Value) {
        sidechain_line(json!({"type": "tool_use", "id": id, "name": "Write", "input": {"file_path": path}}))
    }

    #[test]
    fn a_transcript_with_a_write_but_no_result_has_not_settled() {
        let mut lines = vec![write_use("w1", "/p/sub.txt")];
        assert!(
            !sidechain_writes_have_results(&lines),
            "the call is there, its result is not"
        );
        lines.push(sidechain_line(
            json!({"type": "tool_result", "tool_use_id": "w1", "is_error": true, "content": "refused"}),
        ));
        assert!(sidechain_writes_have_results(&lines));
        // A successful result carries no `is_error` at all and still counts as a result.
        let ok = vec![
            write_use("w2", "/p/sub.txt"),
            sidechain_line(json!({"type": "tool_result", "tool_use_id": "w2", "content": "created"})),
        ];
        assert!(sidechain_writes_have_results(&ok));
        assert_eq!(sidechain_writes(&ok)[0].is_error, Some(false));
    }

    #[test]
    fn a_refused_write_with_no_result_yet_is_unresolved_not_a_leak() {
        let trace = GateTrace::new(Path::new("/p"));
        let pending = sidechain_writes(&[write_use("w1", "/p/sub.txt")]);
        let (ran, unresolved) = sidechain_writes_the_gate_never_saw(&pending, &trace);
        assert!(ran.is_empty(), "no result line is not a success");
        assert_eq!(unresolved.len(), 1);
        // With its error result the same call is neither.
        let refused = sidechain_writes(&[
            write_use("w1", "/p/sub.txt"),
            sidechain_line(json!({"type": "tool_result", "tool_use_id": "w1", "is_error": true, "content": "no"})),
        ]);
        let (ran, unresolved) = sidechain_writes_the_gate_never_saw(&refused, &trace);
        assert!(ran.is_empty() && unresolved.is_empty());
    }

    #[test]
    fn a_write_that_succeeded_without_the_gate_is_a_leak() {
        let trace = GateTrace::new(Path::new("/p"));
        let done = sidechain_writes(&[
            write_use("w1", "/p/sub.txt"),
            sidechain_line(json!({"type": "tool_result", "tool_use_id": "w1", "content": "created"})),
        ]);
        let (ran, unresolved) = sidechain_writes_the_gate_never_saw(&done, &trace);
        assert_eq!(ran.len(), 1);
        assert!(unresolved.is_empty());
        // The same call with a host answer for its id is not.
        let mut seen = GateTrace::new(Path::new("/p"));
        seen.record_answers(&[answer(GateOrigin::GateRequest, "perm-1", "w1", "Write")]);
        let (ran, unresolved) = sidechain_writes_the_gate_never_saw(&done, &seen);
        assert!(ran.is_empty() && unresolved.is_empty());
    }

    #[test]
    fn a_path_is_the_target_only_as_a_path() {
        let root = Path::new("/p");
        assert!(names_the_path("/p/sub.txt", root, Path::new("sub.txt")));
        assert!(names_the_path("sub.txt", root, Path::new("sub.txt")));
        assert!(!names_the_path("/p/dir/sub.txt", root, Path::new("sub.txt")));
        assert!(!names_the_path("/p/notsub.txt", root, Path::new("sub.txt")));
    }
}
