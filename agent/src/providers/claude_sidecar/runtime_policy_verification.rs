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

/// **The tiers a session asks for really decide whether the operator's own `~/.claude` loads --
/// observed at runtime, not inferred from the field being sent.** By default (`[USER, PROJECT,
/// LOCAL]`, what the product sends) the user tier loads, as in a terminal; with
/// `agent.user_settings = false` (`[PROJECT, LOCAL]`) it does not. Three arms: the control (the
/// field absent, which under `NATIVE` means all three tiers) shows the user tier is observable at
/// all; the product's own request must match it; the opt-out request must not.
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
/// the actual thing the field selects, not a synthetic stand-in -- through a value that
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
fn setting_sources_decide_whether_the_user_tier_loads_at_runtime() {
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
    // The second observable, and the decisive one when the declared model is also the CLI's own
    // default (then the model cannot tell the arms apart): a sentence from the user tier's own
    // CLAUDE.md. The user tier is what puts that file into the session's context, so asking the model
    // whether it can see the sentence measures the thing a user notices -- their global instructions
    // being there or not.
    let memory = std::fs::read_to_string(config_dir.join("CLAUDE.md")).unwrap_or_else(|e| {
        panic!(
            "PREMISE FAILED: reading the user tier's CLAUDE.md at {}: {e}",
            config_dir.display()
        )
    });
    let marker = memory
        .lines()
        .map(str::trim)
        .find(|l| !l.starts_with('#') && !l.starts_with('@') && !l.starts_with('|') && l.chars().count() >= 20)
        .map(|l| l.chars().take(120).collect::<String>())
        .expect("PREMISE FAILED: the user tier's CLAUDE.md has no line long enough to quote");
    let ask = format!(
        "Answer with exactly one word, YES or NO. Is the following text part of the instructions or \
         memory files in your context right now?\n\n{marker}"
    );
    eprintln!("marker from the user tier's CLAUDE.md: {marker:?}");

    // ---- CONTROL: setting_sources ABSENT. Under ConfigurationProfile::NATIVE that means all three
    // tiers, so the user tier loads. Without this arm the opt-out arm proves nothing: a tier that
    // never loads looks exactly like a tier that is excluded.
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
            text: ask.clone(),
        })
        .expect("control turn");
    let control_events = drain_until(&control, 120, turn_finished);
    trace("control (setting_sources absent)", &control_events);
    let _ = control.close_session(CloseSessionRequest { session_id });
    drop(control);

    let control_model = model_of(&control_events).unwrap_or_default();
    let control_answer = text_of(&control_events);
    assert!(
        says_yes(&control_answer),
        "POSITIVE CONTROL FAILED: with setting_sources absent the user tier should have loaded, so the \
         model should see the user CLAUDE.md sentence; it answered {control_answer:?}. Fix the control \
         before believing any exclusion result below."
    );
    assert!(
        control_model.contains(&family),
        "POSITIVE CONTROL FAILED: with setting_sources absent the user tier should have loaded and \
         the session should be running the operator's declared model {declared_model:?}; it reports \
         {control_model:?}. Fix the control before believing any exclusion result below."
    );

    // ---- DEFAULT: the real product path. `create_session` goes through `build_create_request`
    // unmodified, which states [USER, PROJECT, LOCAL] (nothing in this test configures the opt-out),
    // so the session must run the operator's declared model, as the control did.
    let provider = connect();
    let session_id = provider
        .create_session(CreateSessionRequest {
            cwd: cwd.path().to_string_lossy().to_string(),
            streaming: StreamingPreference::Complete,
        })
        .expect("default session");
    provider
        .send_turn(SendTurnRequest {
            session_id: session_id.clone(),
            text: ask.clone(),
        })
        .expect("default turn");
    let events = drain_until(&provider, 120, turn_finished);
    trace("default (setting_sources = [USER, PROJECT, LOCAL])", &events);
    let _ = provider.close_session(CloseSessionRequest { session_id });
    drop(provider);

    // The turn really ran. Without this, a model comparison is satisfied by a session that never
    // got as far as starting one.
    assert!(
        turn_finished(&events) && !text_of(&events).trim().is_empty(),
        "the default arm never completed a turn, so its result is not evidence; got: {events:?}"
    );
    let default_model = model_of(&events).unwrap_or_default();
    eprintln!("control model {control_model:?}  vs  default model {default_model:?}");
    let default_answer = text_of(&events);
    assert!(
        says_yes(&default_answer),
        "the default request did NOT load the user tier: the model cannot see the user CLAUDE.md \
         sentence (answered {default_answer:?}). The product's default selection is [USER, PROJECT, LOCAL]."
    );
    assert!(
        default_model.contains(&family),
        "the default request did NOT load the user tier: the session is running {default_model:?}, \
         not the model the operator's own ~/.claude declares ({declared_model:?}). The product's \
         default selection is [USER, PROJECT, LOCAL]."
    );

    // ---- OPT-OUT: the request `agent.user_settings = false` produces. Built through the same
    // function the product uses, with the user tier left out -- the only difference from the default.
    let opted_out = connect();
    let request = build_create_request_loading(
        cwd.path().to_string_lossy().to_string(),
        StreamingPreference::Complete,
        None,
        false,
        opted_out.provider_prompts,
        false,
    );
    let session_id = opted_out.open_session(request).expect("opt-out session");
    opted_out
        .send_turn(SendTurnRequest {
            session_id: session_id.clone(),
            text: ask.clone(),
        })
        .expect("opt-out turn");
    let events = drain_until(&opted_out, 120, turn_finished);
    trace("opt-out (setting_sources = [PROJECT, LOCAL])", &events);
    let _ = opted_out.close_session(CloseSessionRequest { session_id });

    assert!(
        turn_finished(&events) && !text_of(&events).trim().is_empty(),
        "the opt-out arm never completed a turn, so its result is not evidence; got: {events:?}"
    );
    let opt_out_model = model_of(&events).unwrap_or_default();
    eprintln!("control model {control_model:?}  vs  opt-out model {opt_out_model:?}");
    let opt_out_answer = text_of(&events);
    assert!(
        says_no(&opt_out_answer),
        "setting_sources = [PROJECT, LOCAL] did NOT exclude the user tier: the model can see the user \
         CLAUDE.md sentence (answered {opt_out_answer:?}). `agent.user_settings = false` promises that \
         tier stays out."
    );
    // The model is a second, independent observable only when the CLI's own default differs from
    // the declared one; when they coincide (the CLI picked the same family by itself), it says
    // nothing and is reported instead of asserted.
    if opt_out_model.contains(&family) {
        eprintln!(
            "model check not conclusive here: the CLI's own default {opt_out_model:?} is the same \
             family as the declared {declared_model:?}; the CLAUDE.md check above is the evidence"
        );
        return;
    }
    assert!(
        !opt_out_model.contains(&family),
        "setting_sources = [PROJECT, LOCAL] did NOT exclude the user tier: the session is running \
         {opt_out_model:?}, the model the operator's own ~/.claude declares ({declared_model:?}). \
         `agent.user_settings = false` promises that tier stays out."
    );
}

/// Whether a one-word answer says yes (case and punctuation ignored).
fn says_yes(answer: &str) -> bool {
    answer
        .trim()
        .trim_matches(|c: char| !c.is_alphanumeric())
        .eq_ignore_ascii_case("yes")
}

/// Whether a one-word answer says no (case and punctuation ignored).
fn says_no(answer: &str) -> bool {
    answer
        .trim()
        .trim_matches(|c: char| !c.is_alphanumeric())
        .eq_ignore_ascii_case("no")
}

/// The model the provider reported for the session, from the first `SessionOpened`.
fn model_of(events: &[AgentDomainEvent]) -> Option<String> {
    events.iter().find_map(|e| match e {
        AgentDomainEvent::SessionOpened { model, .. } => Some(model.clone()),
        _ => None,
    })
}
