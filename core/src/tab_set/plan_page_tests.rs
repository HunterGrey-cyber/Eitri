//! What a page is told of plan mode beside the conversation: the events envelope's `planningChanged` when the
//! session's phase or the tab's origin moved (alone, when no event came with it), the snapshot's origin, the
//! tab row's feedback drafts and start-in-plan mark.
#![cfg(test)]
use super::*;
use crate::test_providers::RecordingProvider;
use agent::{AgentConversation, AgentDomainEvent};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Page {
    dir: PathBuf,
    set: TabSet,
    tab: TabId,
    provider: Arc<RecordingProvider>,
}

impl Page {
    fn new(label: &str, mode: SessionModeChoice) -> Page {
        Page::on(label, mode, false)
    }

    /// `cli_auto`: the session runs the CLI's own auto mode when it executes.
    fn on(label: &str, mode: SessionModeChoice, cli_auto: bool) -> Page {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir(label);
        let provider = Arc::new(RecordingProvider::planning(cli_auto));
        let conversation =
            AgentConversation::create(provider.clone(), &dir, agent::setting_sources::ProjectTrust::Untrusted).unwrap();
        let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let tab = set.active();
        set.get_mut(tab).unwrap().backend = TabBackend::Live(AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        });
        set.get_mut(tab).unwrap().set_mode_for_test(mode);
        Page {
            dir,
            set,
            tab,
            provider,
        }
    }

    /// Pumps until a payload for the page says `pred`, returning it.
    fn until(&mut self, what: &str, pred: impl Fn(&serde_json::Value) -> bool) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            if let Some(payload) = self.set.pump(&self.dir, true).active_payload {
                let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
                if pred(&value) {
                    return value;
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn shut_down(mut self) {
        for tab in self.set.take_all() {
            if let Some(retired) = tab.retired {
                retired.into_backend().shutdown();
            }
        }
    }
}

#[test]
fn planning_changed_goes_with_the_batch_that_moved_it_and_on_its_own() {
    let mut p = Page::new("plan-page-changed", SessionModeChoice::Bypass);
    p.provider.queue_all(vec![AgentDomainEvent::CliPermissionMode {
        reported: "default".into(),
    }]);
    // The first look at a plan session says where it stands.
    let first = p.until("the first note", |v| v.get("planningChanged").is_some());
    assert_eq!(
        first["planningChanged"],
        serde_json::json!({ "phase": "executing", "origin": null })
    );
    p.provider.queue_all(vec![
        AgentDomainEvent::TurnStarted { turn_id: "t1".into() },
        AgentDomainEvent::ToolCallStarted {
            turn_id: "t1".into(),
            tool_use_id: "toolu_enter".into(),
            name: "EnterPlanMode".into(),
            input: serde_json::json!({}),
        },
        AgentDomainEvent::CliPlanMode {
            source: "CliModeReported".into(),
        },
    ]);
    let planning = p.until("planning", |v| v["planningChanged"]["phase"] == "planning");
    assert_eq!(planning["kind"], "events");
    assert_eq!(planning["planningChanged"]["origin"], "bypass");
    // Nothing changed: no note, whatever else goes out.
    p.provider.queue_all(vec![AgentDomainEvent::ContentDelta {
        turn_id: "t1".into(),
        kind: agent::ContentKind::Text,
        text: "thinking it over".into(),
    }]);
    let quiet = p.until("the text", |v| {
        v["kind"] == "events" && !v["events"].as_array().unwrap().is_empty()
    });
    assert!(quiet.get("planningChanged").is_none(), "{quiet}");
    // The snapshot a switch or a reload gets carries the origin too.
    let snapshot: serde_json::Value = serde_json::from_str(
        &p.set
            .active_state_payloads()
            .into_iter()
            .find(|s| s.contains("\"snapshot\""))
            .expect("a snapshot"),
    )
    .unwrap();
    assert_eq!(
        snapshot["state"]["planning"],
        serde_json::json!({ "phase": "planning", "origin": "bypass" })
    );
    assert_eq!(snapshot["state"]["capabilities"]["planMode"], true);
    p.shut_down();
}

/// An auto session's CLI that stays at `default` on its way out of plan mode is its fallback out of auto: the
/// guard ends the move with no event of the session's, and the page is still told, in an envelope of its own.
#[test]
fn a_move_the_guard_ends_reaches_the_page_on_its_own() {
    let mut p = Page::on("plan-page-alone", SessionModeChoice::Auto, true);
    p.provider.queue_all(vec![AgentDomainEvent::CliPermissionMode {
        reported: "auto".into(),
    }]);
    p.until("the first note", |v| v.get("planningChanged").is_some());
    let enter = p.set.set_planning(p.tab, ViewId::BASE, true, 0).unwrap().unwrap();
    assert!(p.set.finish_planning_switch(p.tab, &enter).unwrap().is_ok());
    p.provider.queue_all(vec![AgentDomainEvent::CliPlanMode {
        source: "CliModeReported".into(),
    }]);
    p.until("planning", |v| v["planningChanged"]["phase"] == "planning");
    let leave = p.set.set_planning(p.tab, ViewId::BASE, false, 0).unwrap().unwrap();
    assert!(p.set.finish_planning_switch(p.tab, &leave).unwrap().is_ok());
    p.provider.queue_all(vec![AgentDomainEvent::CliPermissionMode {
        reported: "default".into(),
    }]);
    p.until("leaving", |v| v["planningChanged"]["phase"] == "leaving");
    // The CLI's `default` is folded, and the page told of everything up to it, before the guard looks.
    p.provider.wait_until_folded();
    p.set.pump(&p.dir, true);
    assert!(matches!(
        p.set.get(p.tab).unwrap().live().unwrap().projection().planning.phase,
        agent::PlanPhase::Leaving { saw_default: true, .. }
    ));
    let tripped = p.set.check_plan_transitions(wall_clock_ms() + 60_000);
    assert!(tripped.is_empty(), "the fallback is no failure");
    let alone = p.until("executing", |v| v["planningChanged"]["phase"] == "executing");
    assert_eq!(alone["events"], serde_json::json!([]), "{alone}");
    p.shut_down();
}

#[test]
fn the_tab_row_says_plan_on_start_and_carries_the_feedback_draft() {
    let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    let tab = set.active();
    assert_eq!(set.set_planning(tab, ViewId::BASE, true, 0).unwrap().is_none(), true);
    let row = |set: &TabSet| {
        let wire: serde_json::Value = serde_json::from_str(&set.tabs_payload()).unwrap();
        wire["tabs"][0].clone()
    };
    assert_eq!(row(&set)["planOnStart"], true);
    set.set_planning(tab, ViewId::BASE, false, 0).unwrap();
    assert!(row(&set).get("planOnStart").is_none());
    // A draft needs a waiting plan.
    assert!(set
        .set_plan_feedback_draft(tab, ViewId::BASE, "p1", "x".into())
        .is_err());
    let mut p = Page::new("plan-page-draft", SessionModeChoice::Manual);
    p.provider.queue_all(vec![
        AgentDomainEvent::TurnStarted { turn_id: "t1".into() },
        AgentDomainEvent::PlanProposed {
            interaction_id: "p1".into(),
            tool_use_id: Some("toolu_exit".into()),
            plan: "# Plan".into(),
            plan_sha256: "0".repeat(64),
            allowed_prompts: Vec::new(),
            unsupported_reason: None,
        },
    ]);
    p.until("the plan", |v| v.to_string().contains("plan_proposed"));
    let too_long = "x".repeat(agent::PLAN_FEEDBACK_MAX_BYTES + 1);
    assert!(p
        .set
        .set_plan_feedback_draft(p.tab, ViewId::BASE, "p1", too_long)
        .is_err());
    assert!(p
        .set
        .set_plan_feedback_draft(p.tab, ViewId::new(2).unwrap(), "p1", "x".into())
        .is_err());
    p.set
        .set_plan_feedback_draft(p.tab, ViewId::BASE, "p1", "shorter".into())
        .unwrap();
    assert_eq!(row(&p.set)["planFeedbackDraft"], serde_json::json!({ "p1": "shorter" }));
    p.set
        .set_plan_feedback_draft(p.tab, ViewId::BASE, "p1", String::new())
        .unwrap();
    assert!(row(&p.set).get("planFeedbackDraft").is_none(), "empty text drops it");
    p.set
        .set_plan_feedback_draft(p.tab, ViewId::BASE, "p1", "again".into())
        .unwrap();
    // Dropped when the plan settles.
    p.provider.queue_all(vec![AgentDomainEvent::PlanSettled {
        interaction_id: "p1".into(),
        outcome: agent::PlanOutcome::Interrupted,
        mode_after: None,
        feedback: None,
        reason: None,
    }]);
    p.until("the settlement", |v| v.to_string().contains("plan_settled"));
    p.set.pump(&p.dir, true);
    assert!(row(&p.set).get("planFeedbackDraft").is_none());
    p.shut_down();
}

/// A tab whose session stops offering plan mode (its backend replaced) tells the page so, as
/// `planningChanged: null`, rather than leaving plan mode on the page until the next snapshot.
#[test]
fn a_planning_note_that_goes_away_is_said_as_null() {
    let mut p = Page::new("plan-page-cleared", SessionModeChoice::Auto);
    p.provider.queue_all(vec![AgentDomainEvent::CliPermissionMode {
        reported: "default".into(),
    }]);
    p.until("the first note", |v| v.get("planningChanged").is_some());
    let tab = p.set.get_mut(p.tab).unwrap();
    assert!(!tab.notes.planning_dirty, "the first note went out");
    let live = std::mem::replace(&mut tab.backend, TabBackend::NotStarted);
    TabSet::note_planning(tab);
    assert_eq!(tab.notes.planning, None);
    assert!(tab.notes.planning_dirty, "the change is owed to the page");
    tab.backend = live;
    let cleared = crate::agent_bridge::CallNotes {
        planning_cleared: true,
        ..Default::default()
    };
    assert!(!cleared.is_empty());
    let payload: serde_json::Value = serde_json::from_str(
        &crate::agent_bridge::serialize_events_with_notes_and_revisions_for_js(p.tab, 3, 3, &[], &cleared, None),
    )
    .unwrap();
    assert!(
        payload["planningChanged"].is_null() && payload.get("planningChanged").is_some(),
        "{payload}"
    );
    p.shut_down();
}

/// The page tells a cycle refused because plan mode holds the tab from any other refusal by these words
/// (`agent-ui/web/src/fold/plans.ts`, `PLAN_MODE_REFUSALS`), so the two must stay the same.
#[test]
fn the_page_knows_the_plan_mode_refusals_word_for_word() {
    let page = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../agent-ui/web/src/fold/plans.ts"
    ))
    .unwrap();
    let list = page
        .split("PLAN_MODE_REFUSALS: readonly string[] = [")
        .nth(1)
        .and_then(|rest| rest.split(']').next())
        .expect("the page's list of plan-mode refusals");
    for refusal in [PLAN_CYCLE_REFUSAL, PLAN_CHANGING_REFUSAL] {
        assert!(
            list.contains(&format!("{refusal:?}")),
            "{refusal:?} is not in the page's list: {list}"
        );
    }
    assert_eq!(list.matches('"').count(), 4, "the page names exactly these two: {list}");
}

#[test]
fn a_background_waiting_plan_is_marked_counted_and_reachable_until_settled() {
    let mut p = Page::new("plan-attention", SessionModeChoice::Manual);
    let foreground = p.set.open(AgentId::Claude);
    assert_eq!(p.set.active(), foreground);
    assert_ne!(p.set.active(), p.tab);
    assert!(p.set.get(p.tab).unwrap().view.is_base());
    p.provider.queue_all(vec![AgentDomainEvent::PlanProposed {
        interaction_id: "p1".into(),
        tool_use_id: None,
        plan: "# Plan".into(),
        plan_sha256: "0".repeat(64),
        allowed_prompts: Vec::new(),
        unsupported_reason: None,
    }]);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline);
        p.set.pump(&p.dir, true);
        if p.set.get(p.tab).unwrap().plan_waiting() {
            break;
        }
        std::thread::yield_now();
    }
    p.set.pump(&p.dir, true);
    assert_eq!(
        tabs::marker(p.set.get(p.tab).unwrap().facts()),
        Some(tabs::Marker::NeedsInput(1))
    );
    assert!(!p.set.get(foreground).unwrap().attention.attention().unread);
    assert_eq!(p.set.get(p.tab).unwrap().attention.attention().pending, 1);
    assert_eq!(p.set.attention().pending, 1);
    assert_eq!(
        crate::layout::tray::chip_label("agent", Some(p.set.attention())),
        "agent ⚑1"
    );
    assert_eq!(p.set.oldest_card_tab(), Some(p.tab));
    assert!(p.set.get(p.tab).unwrap().attention.card_ids().is_empty());
    let context = super::agent_rows::RowContext {
        instance: "0123456789abcdef".into(),
        project: crate::agents::ProjectRef::default(),
        window: crate::agents::WindowRef::default(),
    };
    let rows = p.set.agent_rows(&context, &|id| id == foreground, 0);
    let row = rows
        .iter()
        .find(|row| row.number == p.set.get(p.tab).unwrap().number)
        .unwrap();
    assert!(!row.shown);
    assert_eq!(row.state, "question");
    assert_eq!(row.class, crate::agents::Class::Needs);
    assert_eq!(row.needs.questions, 1);

    p.provider.queue_all(vec![AgentDomainEvent::PlanSettled {
        interaction_id: "p1".into(),
        outcome: agent::PlanOutcome::Interrupted,
        mode_after: None,
        feedback: None,
        reason: None,
    }]);
    let deadline = Instant::now() + Duration::from_secs(30);
    while p.set.get(p.tab).unwrap().plan_waiting() {
        assert!(Instant::now() < deadline);
        p.set.pump(&p.dir, true);
        std::thread::yield_now();
    }
    p.set.pump(&p.dir, true);
    assert_eq!(p.set.attention().pending, 0);
    assert_eq!(p.set.oldest_card_tab(), None);
    let rows = p.set.agent_rows(&context, &|id| id == foreground, 1);
    let row = rows
        .iter()
        .find(|row| row.number == p.set.get(p.tab).unwrap().number)
        .unwrap();
    assert_eq!(row.needs.questions, 0);
    assert_ne!(row.state, "question");
    p.shut_down();
}
