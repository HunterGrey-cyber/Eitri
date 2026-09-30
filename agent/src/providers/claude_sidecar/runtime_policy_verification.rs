// agent/src/providers/claude_sidecar/runtime_policy_verification.rs
//! **Every test in this file spends real Claude tokens and must be run against the TEST Claude
//! profile, via a wrapper that runs it on a dedicated test account** (see `docs/canonical/dated_record.md`'s
//! 2026-09-19 entry for why that replaced billing work).
//!
//! **Record the CLI build in the same breath, because nothing does it for you.** The test-account wrapper
//! routes both doors a harness can use through the policy launcher, but it pins no version for the
//! length of a run and echoes none; the tool it replaced did both. What these tests establish is a
//! *runtime* boundary, and a runtime boundary whose build is unknown is precisely the defect
//! 2026-09-18 spent a day finding -- it cost this exact A/B pair a re-run. So run the first line
//! and keep its answer beside the result.
//!
//! ```text
//! claude --version
//! EITRI_VERDANDI_CHECKOUT=$HOME/src/verdandi-old-checkout \
//!   cargo test -p agent --lib runtime_policy -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! ## Why these are unit tests and not integration tests
//!
//! Protocol 3 gave this client two fields that *narrow* what a spawned agent may do --
//! `ClaudeHostPolicy.setting_sources` and `ClaudeHostPolicy.tool_policy` -- and until this file
//! existed both were established only as far as "the value reaches the SDK's `Options` object".
//! That is not evidence about runtime behavior, and the next piece of product work (letting the
//! agent edit files) *widens* what it may do. A constraint you have not run is not a constraint.
//!
//! Proving one needs an **A/B pair**, because an absence on its own proves nothing: a hook that
//! never fires and a hook that was never reachable look identical, and a tool the model declined to
//! call looks exactly like a tool it was forbidden to call. So each test below runs two arms that
//! differ in **exactly one proto field**:
//!
//! - the CONTROL arm hand-builds a request via `build_create_request` and edits that one field;
//! - the TEST arm goes through `ClaudeSidecarProvider::create_session` -- the real product path,
//!   carrying the real `CONSERVATIVE_DISALLOWED_TOOLS` and the real `[PROJECT, LOCAL]` selection.
//!
//! **Since R07 (2026-09-27) every session here is gated** (`INTERACTIVE`, never switchable): the
//! product sends nothing else, and `build_create_request` no longer takes a mode. The second test
//! this file used to hold, `tool_policy_deny_really_stops_a_bypass_session_from_running_bash`, was
//! deleted rather than ported: it measured a BYPASS session's deny list, BYPASS is no longer
//! something this client can create, and it had been asserting a `Bash` denial the product stopped
//! sending on 2026-09-20. Its A/B result stays in `agent/MANUAL_VERIFICATION.md`, 2026-09-15 (later).
//!
//! `build_create_request` and `open_session` are private, deliberately (the policy is fixed rather
//! than parameterised so the product cannot send a value nobody has confirmed the meaning of). A
//! test that reached them through a `pub` seam would have widened the very API that design protects,
//! so these live inside the crate instead. That is also why they are `--lib` tests: **`cargo test
//! -p agent --lib -- --ignored` now bills.** It did already, via `agent::process`'s real-CLI tests;
//! this file adds model turns to that set.

use super::*;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// A throwaway directory used as the session's `cwd`. Fresh on purpose: a `cwd` with a real
/// `.claude/` in it would let the PROJECT or LOCAL tier supply whatever the USER tier was supposed
/// to, and the test would not be able to tell which one it observed.
struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!("eitri-{label}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("scratch dir");
        Scratch(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn connect() -> ClaudeSidecarProvider {
    ClaudeSidecarProvider::connect(&uuid::Uuid::new_v4().to_string())
        .expect("connecting to a real sidecar should succeed")
}

/// Drains `pump()` until `done` or the deadline, accumulating everything seen. Same polling idiom as
/// `agent/tests/claude_sidecar_lifecycle_conformance.rs` rather than a second one.
fn drain_until<F: Fn(&[AgentDomainEvent]) -> bool>(
    provider: &ClaudeSidecarProvider,
    deadline_secs: u64,
    done: F,
) -> Vec<AgentDomainEvent> {
    let deadline = Instant::now() + Duration::from_secs(deadline_secs);
    let mut all = Vec::new();
    while Instant::now() < deadline {
        all.extend(provider.pump());
        if done(&all) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    all
}

fn turn_finished(events: &[AgentDomainEvent]) -> bool {
    events
        .iter()
        .any(|e| matches!(e, AgentDomainEvent::TurnCompleted { .. }))
}

fn text_of(events: &[AgentDomainEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            AgentDomainEvent::ContentDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// Prints the event trace unconditionally. Kept permanently: when one of these fails, the sequence
/// the provider actually produced is the first thing anyone needs, and reconstructing it costs
/// another billed run.
fn trace(label: &str, events: &[AgentDomainEvent]) {
    eprintln!("---- {label}: {} events ----", events.len());
    for event in events {
        match event {
            AgentDomainEvent::SessionOpened {
                provider_session_id,
                model,
                ..
            } => {
                eprintln!("  SessionOpened   provider_session_id={provider_session_id} model={model}")
            }
            AgentDomainEvent::ContentDelta { text, .. } => {
                eprintln!("  ContentDelta    {:?}", text.chars().take(60).collect::<String>())
            }
            AgentDomainEvent::ToolCallStarted { name, tool_use_id, .. } => {
                eprintln!("  ToolCallStarted {name} tool_use_id={tool_use_id}")
            }
            AgentDomainEvent::ToolCallCompleted {
                tool_use_id, is_error, ..
            } => {
                eprintln!("  ToolCallDone    tool_use_id={tool_use_id} is_error={is_error}")
            }
            AgentDomainEvent::TurnCompleted { outcome, .. } => eprintln!("  TurnCompleted   {outcome:?}"),
            other => eprintln!("  {other:?}"),
        }
    }
}

/// **`setting_sources: [PROJECT, LOCAL]` really keeps the operator's own `~/.claude` out of a
/// spawned session -- observed at runtime, not inferred from the field being sent.**
///
/// The observable is the **session model**, and the reason it is that rather than a hook is worth
/// more than the result. The first version of this test installed its own `UserPromptSubmit` marker
/// hook in a throwaway directory and pointed `CLAUDE_CONFIG_DIR` at it. That cannot work, and the
/// run that proved it is the only reason anyone knows: the sidecar resolves the account **once, for
/// the whole process**, from `VERDANDI_CLAUDE_ACCOUNT`, and sets `CLAUDE_CONFIG_DIR` for every
/// subprocess itself (`apps/claude-sidecar/src/index.ts`). A caller's own value is overridden, and
/// silently -- the marker simply never appeared. **A test cannot redirect the user tier while an
/// account is pinned.**
///
/// What that failure handed back is a better test. It measures the operator's *real* user tier --
/// the actual thing the field exists to exclude, not a synthetic stand-in -- through a value that
/// reaches `pump()`: `SessionOpened.model`. `settings.json`'s `model` key is a user-tier setting and
/// nothing else in a fresh `cwd` supplies one.
///
/// The test states its own premise rather than assuming it. If the operator's settings stop setting
/// a model, the control fails saying so, instead of quietly comparing two identical defaults and
/// reporting success.
///
/// Corroborating, and deliberately not asserted on: under the control the provider also logs
/// `hook_started`/`hook_response` notices -- the operator's own `UserPromptSubmit` hooks running
/// inside a spawned session. `ProviderNotice` is diagnostics-only by design (`translate.rs`) and
/// never becomes a domain event, so it can be read in `--nocapture` output but not asserted.
#[test]
#[ignore]
fn setting_sources_project_local_really_excludes_the_user_tier_at_runtime() {
    let cwd = Scratch::new("cwd");

    // The operator's real user tier, as the sidecar will resolve it for its subprocesses.
    let config_dir = std::env::var("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap()).join(".claude"));
    let settings: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(config_dir.join("settings.json"))
            .unwrap_or_else(|e| panic!("reading the user tier at {}: {e}", config_dir.display())),
    )
    .expect("the user tier's settings.json should be valid JSON");
    let declared_model = settings
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or_else(|| {
            panic!(
                "PREMISE FAILED: {}/settings.json sets no `model`, so the user tier has no effect \
                 this test can observe through pump(). Point this test at another user-tier setting \
                 with a wire-visible consequence, or restore one.",
                config_dir.display()
            )
        })
        .to_string();
    // "opus[1m]" in settings comes back as "claude-opus-5[1m]" on the wire: the CLI resolves the
    // alias. Compare on the family token, which survives that resolution.
    let family = declared_model.split('[').next().unwrap().to_string();
    eprintln!(
        "user tier: {} declares model {declared_model:?} (family {family:?})",
        config_dir.display()
    );

    // ---- CONTROL: setting_sources ABSENT. Under ConfigurationProfile::NATIVE that means all three
    // tiers, so the user tier loads. Without this arm the test arm proves nothing: a tier that never
    // loads looks exactly like a tier that is excluded.
    let control = connect();
    // Gated, as every product session is (R07); this turn uses no tool, so nothing is asked.
    let mut request = build_create_request(
        cwd.path().to_string_lossy().to_string(),
        StreamingPreference::Complete,
        None,
        false,
        control.provider_prompts,
    );
    request.policy.as_mut().unwrap().setting_sources = None;
    let session_id = control.open_session(request).expect("control session");
    control
        .send_turn(SendTurnRequest {
            session_id: session_id.clone(),
            text: "Reply with exactly: ok".into(),
        })
        .expect("control turn");
    let control_events = drain_until(&control, 120, turn_finished);
    trace("control (setting_sources absent)", &control_events);
    let _ = control.close_session(CloseSessionRequest { session_id });
    drop(control);

    let control_model = model_of(&control_events).unwrap_or_default();
    assert!(
        control_model.contains(&family),
        "POSITIVE CONTROL FAILED: with setting_sources absent the user tier should have loaded and \
         the session should be running the operator's declared model {declared_model:?}; it reports \
         {control_model:?}. Fix the control before believing any exclusion result below."
    );

    // ---- TEST: the real product path. `create_session` goes through `build_create_request`
    // unmodified, which states [PROJECT, LOCAL] -- the only difference from the control above.
    let provider = connect();
    let session_id = provider
        .create_session(CreateSessionRequest {
            cwd: cwd.path().to_string_lossy().to_string(),
            streaming: StreamingPreference::Complete,
        })
        .expect("test session");
    provider
        .send_turn(SendTurnRequest {
            session_id: session_id.clone(),
            text: "Reply with exactly: ok".into(),
        })
        .expect("test turn");
    let events = drain_until(&provider, 120, turn_finished);
    trace("test (setting_sources = [PROJECT, LOCAL])", &events);
    let _ = provider.close_session(CloseSessionRequest { session_id });

    // The turn really ran. Without this, "the user tier had no effect" is satisfied by a session
    // that never got as far as starting one.
    assert!(
        turn_finished(&events) && !text_of(&events).trim().is_empty(),
        "the test arm never completed a turn, so its result is not evidence; got: {events:?}"
    );
    let test_model = model_of(&events).unwrap_or_default();
    eprintln!("control model {control_model:?}  vs  test model {test_model:?}");
    assert!(
        !test_model.contains(&family),
        "setting_sources = [PROJECT, LOCAL] did NOT exclude the user tier: the session is running \
         {test_model:?}, the model the operator's own ~/.claude declares ({declared_model:?}). That \
         tier carries their hooks and plugins too, and this is the exact inheritance the field \
         exists to prevent."
    );
}

/// The model the provider reported for the session, from the first `SessionOpened`.
fn model_of(events: &[AgentDomainEvent]) -> Option<String> {
    events.iter().find_map(|e| match e {
        AgentDomainEvent::SessionOpened { model, .. } => Some(model.clone()),
        _ => None,
    })
}
