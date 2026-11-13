//! The gate and the tab's mode while a Claude session plans: the plan table, the hold while the mode changes,
//! the person's own switch, the decision on a plan and the mode the tab takes up afterwards. Every path that
//! answers cards by itself is checked to answer nothing a plan-mode card asks.
#![cfg(test)]
use super::*;
use crate::agent_backend::{plan_verdict, PlanGate, PlanVerdict};
use crate::test_providers::RecordingProvider;
use agent::{
    AgentConversation, AgentDomainEvent, PermissionDecision, PermissionMode, PlanExecutionMode, PlanOutcome, PlanPhase,
    PlanState,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

const SHA: &str = "00000000000000000000000000000000000000000000000000000000000000aa";

fn workspace(label: &str) -> PathBuf {
    agent::state_dirs::redirect_state_to_a_test_root();
    let dir = agent::state_dirs::test_workspace_dir(label);
    std::fs::write(dir.join("main.rs"), "fn main() {}").unwrap();
    dir
}

/// A tab in `mode` on a live session that offers plan mode; `cli_auto` is whether the session runs the CLI's
/// own auto mode when it executes.
struct Plan {
    dir: PathBuf,
    set: TabSet,
    tab: TabId,
    provider: Arc<RecordingProvider>,
}

impl Plan {
    fn new(label: &str, cli_auto: bool, mode: SessionModeChoice) -> Plan {
        let dir = workspace(label);
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
        let mut plan = Plan {
            dir,
            set,
            tab,
            provider,
        };
        plan.queue(vec![AgentDomainEvent::CliPermissionMode {
            reported: if cli_auto { "auto" } else { "default" }.into(),
        }]);
        plan.pump_until("the CLI's mode", |p| p.cli_mode_reported.is_some());
        plan
    }

    fn queue(&self, events: Vec<AgentDomainEvent>) {
        self.provider.queue_all(events);
        self.provider.wait_until_folded();
    }

    fn pump(&mut self) {
        self.provider.wait_until_folded();
        self.set.pump(&self.dir, true);
    }

    fn pump_until(&mut self, what: &str, done: impl Fn(&agent::AgentSessionProjection) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            self.set.pump(&self.dir, true);
            if done(&self.backend().projection()) {
                self.set.pump(&self.dir, true);
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn backend(&self) -> &AgentBackend {
        self.set.get(self.tab).unwrap().live().unwrap()
    }

    fn tab(&self) -> &Tab {
        self.set.get(self.tab).unwrap()
    }

    fn phase(&self) -> PlanPhase {
        self.backend().projection().planning.phase.clone()
    }

    /// The model enters plan mode in a running turn, and the CLI says so.
    fn enter(&mut self) {
        self.queue(vec![
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
        self.pump_until("planning", |p| p.planning.phase == PlanPhase::Planning);
        assert_eq!(self.tab().plan_origin(), Some(self.tab().mode()));
    }

    /// The model proposes a plan, `p1`, in the running turn.
    fn propose(&mut self) {
        self.queue(vec![AgentDomainEvent::PlanProposed {
            interaction_id: "p1".into(),
            tool_use_id: Some("toolu_exit".into()),
            plan: "# Plan\n\n1. Do it".into(),
            plan_sha256: SHA.into(),
            allowed_prompts: Vec::new(),
            unsupported_reason: None,
        }]);
        self.pump_until("the plan", |p| p.waiting_plan().is_some());
    }

    fn settle(&mut self, outcome: PlanOutcome, mode_after: Option<PlanExecutionMode>) {
        self.queue(vec![AgentDomainEvent::PlanSettled {
            interaction_id: "p1".into(),
            outcome,
            mode_after,
            feedback: None,
            reason: None,
        }]);
        self.pump();
    }

    /// The CLI reports it is back in the session's creation mode.
    fn cli_back(&mut self, creation: &str) {
        self.queue(vec![AgentDomainEvent::CliPermissionMode {
            reported: creation.into(),
        }]);
        self.pump_until("executing", |p| p.planning.phase == PlanPhase::Executing);
    }

    /// The CLI asks about the main thread's call `toolu_<id>`: the call starts first in the running turn, as
    /// on the wire, unless it already has.
    fn ask(&mut self, id: &str, tool: &str, input: serde_json::Value, prompt: bool) {
        let call = format!("toolu_{id}");
        let turn = {
            let projection = self.backend().projection();
            let started = projection.tool_calls.iter().any(|c| c.tool_use_id == call);
            projection.active_turn_id.clone().filter(|_| !started)
        };
        let mut events = Vec::new();
        if let Some(turn_id) = turn {
            events.push(AgentDomainEvent::ToolCallStarted {
                turn_id,
                tool_use_id: call,
                name: tool.into(),
                input: input.clone(),
            });
        }
        events.push(request(id, tool, input, prompt));
        self.queue(events);
        self.pump();
        self.pump();
    }

    fn decision(&self, id: &str) -> Option<PermissionDecision> {
        self.provider
            .decisions()
            .into_iter()
            .find(|(answered, _)| answered == id)
            .map(|(_, decision)| decision)
    }

    fn carded(&self, id: &str) -> bool {
        self.tab().attention.card_ids().contains(&id.to_string())
    }

    fn shut_down(mut self) {
        for tab in self.set.take_all() {
            if let Some(retired) = tab.retired {
                retired.into_backend().shutdown();
            }
        }
    }
}

fn request(id: &str, tool: &str, input: serde_json::Value, prompt: bool) -> AgentDomainEvent {
    AgentDomainEvent::PermissionRequested {
        permission_id: id.into(),
        tool_use_id: Some(format!("toolu_{id}")),
        tool_name: tool.into(),
        input,
        provider_prompt: prompt.then(|| agent::ProviderPrompt {
            reason: Some("Claude Code asks before this in plan mode".into()),
            description: None,
            blocked_path: None,
            matched_ask_rule: None,
            unrecognized_origin: None,
        }),
    }
}

fn write() -> serde_json::Value {
    serde_json::json!({ "file_path": "notes.md", "content": "x" })
}

fn bash(command: &str) -> serde_json::Value {
    serde_json::json!({ "command": command })
}

const ORIGINS: [SessionModeChoice; 3] = [
    SessionModeChoice::Manual,
    SessionModeChoice::Auto,
    SessionModeChoice::Bypass,
];

#[test]
fn the_verdict_per_origin_matches_the_table() {
    use PermissionMode::{Auto, Bypass, Manual};
    let gate = |origin, cli_runs_auto| PlanGate {
        origin,
        cli_runs_auto,
        main_thread: true,
    };
    let subagent = |origin| PlanGate {
        origin,
        cli_runs_auto: true,
        main_thread: false,
    };
    for origin in [Manual, Auto, Bypass] {
        for tool in agent_backend_plan_tools() {
            // A main-thread gate request for `Write` or `Bash`: left to the CLI's plan mode, which was seen to
            // ask before either; `Edit` and `NotebookEdit`, which nobody saw, are cards. A card at a root that is
            // no boundary, and for a call not tied to the main thread; never an answer the host gives itself.
            let measured = matches!(tool, "Write" | "Bash");
            assert_eq!(
                plan_verdict(tool, false, gate(origin, true), true),
                if measured {
                    PlanVerdict::Defer
                } else {
                    PlanVerdict::Card
                },
                "{tool}"
            );
            assert_eq!(
                plan_verdict(tool, false, gate(origin, true), false),
                PlanVerdict::Card,
                "{tool}"
            );
            assert_eq!(
                plan_verdict(tool, false, subagent(origin), true),
                PlanVerdict::Card,
                "{tool}"
            );
            // The CLI's own prompt for it: a card in every origin, Bypass included.
            assert_eq!(
                plan_verdict(tool, true, gate(origin, true), true),
                PlanVerdict::Card,
                "{tool}"
            );
        }
        assert_eq!(
            plan_verdict("WebFetch", true, gate(origin, false), true),
            PlanVerdict::Card
        );
    }
    assert_eq!(
        plan_verdict("Read", false, gate(Manual, true), true),
        PlanVerdict::AsMode(Manual)
    );
    assert_eq!(
        plan_verdict("Read", false, gate(Auto, true), true),
        PlanVerdict::AsMode(Auto)
    );
    assert_eq!(
        plan_verdict("Read", false, gate(Bypass, true), true),
        PlanVerdict::Defer
    );
    assert_eq!(
        plan_verdict("Read", false, gate(Bypass, false), true),
        PlanVerdict::Card
    );
    assert_eq!(
        plan_verdict("Read", false, gate(Bypass, true), false),
        PlanVerdict::Card
    );
}

fn agent_backend_plan_tools() -> [&'static str; 4] {
    crate::agent_backend::PLAN_ASKED_TOOLS
}

#[test]
fn write_and_bash_are_deferred_in_plan_in_every_origin() {
    for origin in ORIGINS {
        let mut p = Plan::new("plan-write-deferred", true, origin);
        p.enter();
        p.ask("w", "Write", write(), false);
        p.ask("b", "Bash", bash("python3 x.py"), false);
        p.ask("r", "Bash", bash("ls"), false);
        for id in ["w", "b", "r"] {
            assert_eq!(p.decision(id), Some(PermissionDecision::Defer), "{origin:?} {id}");
            assert!(!p.carded(id), "{origin:?} {id}");
        }
        p.shut_down();
    }
}

/// The CLI asks about a call that writes or runs while it plans: a card in every origin, Bypass included,
/// and no route that answers cards by itself allows it, before or after plan mode ends.
#[test]
fn a_provider_prompt_for_the_four_tools_in_plan_is_a_card_in_every_origin() {
    for origin in ORIGINS {
        let mut p = Plan::new("plan-prompt-card", true, origin);
        p.enter();
        for (id, tool, input) in [
            ("w", "Write", write()),
            (
                "e",
                "Edit",
                serde_json::json!({ "file_path": "main.rs", "old_string": "a", "new_string": "b" }),
            ),
            (
                "n",
                "NotebookEdit",
                serde_json::json!({ "notebook_path": "a.ipynb", "new_source": "" }),
            ),
            ("b", "Bash", bash("touch y")),
        ] {
            p.ask(id, tool, input, true);
            assert_eq!(p.decision(id), None, "{origin:?} {tool}");
            assert!(p.carded(id), "{origin:?} {tool}");
        }
        // Once plan mode has ended, in whatever mode the tab is then (Bypass for a Bypass round), nothing that
        // answers cards by itself takes one: entering bypass would approve none, and the pump answers none.
        p.propose();
        let mode_after = if origin == SessionModeChoice::Bypass {
            PlanExecutionMode::Bypass
        } else {
            PlanExecutionMode::Auto
        };
        p.settle(PlanOutcome::Approved, Some(mode_after));
        p.cli_back("auto");
        p.pump();
        p.pump();
        assert_eq!(p.phase(), PlanPhase::Executing);
        assert!(
            waiting_cards(p.tab()).is_empty(),
            "{origin:?}: bypass would approve none of them"
        );
        for id in ["w", "e", "n", "b"] {
            assert_eq!(p.decision(id), None, "{origin:?} {id}");
            assert!(p.carded(id), "{origin:?} {id}");
        }
        p.shut_down();
    }
}

#[test]
fn an_ordinary_provider_prompt_in_plan_needs_a_human_even_after_approval_cache() {
    let mut p = Plan::new("plan-prompt-cache", false, SessionModeChoice::Manual);
    p.enter();
    // Manual cards the fetch; the person approves its card.
    p.ask(
        "g",
        "WebFetch",
        serde_json::json!({ "url": "https://example.com" }),
        false,
    );
    assert!(p.carded("g"));
    p.set
        .answer_card(p.tab, ViewId::BASE, "g", PermissionDecision::Allow)
        .unwrap();
    // The CLI's own prompt for that very call is still a card while it plans.
    p.queue(vec![AgentDomainEvent::PermissionRequested {
        permission_id: "g2".into(),
        tool_use_id: Some("toolu_g".into()),
        tool_name: "WebFetch".into(),
        input: serde_json::json!({ "url": "https://example.com" }),
        provider_prompt: Some(agent::ProviderPrompt {
            reason: Some("fetching".into()),
            description: None,
            blocked_path: None,
            matched_ask_rule: None,
            unrecognized_origin: None,
        }),
    }]);
    p.pump();
    p.pump();
    assert_eq!(p.decision("g2"), None);
    assert!(p.carded("g2"));
    p.shut_down();
}

#[test]
fn bypass_origin_never_allows_by_bypass_or_a_saved_rule_in_plan() {
    let mut p = Plan::new("plan-bypass-auto", true, SessionModeChoice::Bypass);
    p.set
        .set_rules(agent::PrefixRules::default().with(agent::PrefixRule::parse("Bash(cargo test *)").unwrap()));
    p.enter();
    p.ask("r", "Read", serde_json::json!({ "file_path": "main.rs" }), false);
    p.ask("t", "Bash", bash("cargo test"), false);
    assert_eq!(p.decision("r"), Some(PermissionDecision::Defer));
    assert_eq!(
        p.decision("t"),
        Some(PermissionDecision::Defer),
        "no saved rule allows in plan mode"
    );
    assert!(p.provider.decisions().iter().all(|(_, d)| !d.allows()));
    p.shut_down();
    // On a session whose CLI does not run its own auto mode, Bypass leaves nothing to it: a card.
    let mut q = Plan::new("plan-bypass-default", false, SessionModeChoice::Bypass);
    q.enter();
    q.ask("r", "Read", serde_json::json!({ "file_path": "main.rs" }), false);
    assert_eq!(q.decision("r"), None);
    assert!(q.carded("r"));
    q.shut_down();
}

#[test]
fn a_write_beside_enter_plan_mode_in_manual_is_a_card_not_deferred() {
    let mut p = Plan::new("plan-entering-write", true, SessionModeChoice::Manual);
    p.queue(vec![
        AgentDomainEvent::TurnStarted { turn_id: "t1".into() },
        AgentDomainEvent::ToolCallStarted {
            turn_id: "t1".into(),
            tool_use_id: "toolu_enter".into(),
            name: "EnterPlanMode".into(),
            input: serde_json::json!({}),
        },
    ]);
    p.pump();
    assert!(matches!(p.phase(), PlanPhase::Entering { .. }));
    p.ask("w", "Write", write(), false);
    assert_eq!(
        p.decision("w"),
        None,
        "the CLI still runs its earlier mode: Manual's own verdict"
    );
    assert!(p.carded("w"));
    p.shut_down();
}

#[test]
fn manual_origin_never_defers_outside_plan_gate() {
    let mut p = Plan::new("plan-manual-executing", true, SessionModeChoice::Manual);
    p.queue(vec![AgentDomainEvent::TurnStarted { turn_id: "t1".into() }]);
    p.pump();
    p.ask("r", "Read", serde_json::json!({ "file_path": "main.rs" }), false);
    p.ask("w", "Write", write(), false);
    assert_eq!(p.decision("r"), Some(PermissionDecision::Allow));
    assert_eq!(p.decision("w"), None);
    assert!(p.carded("w"));
    p.shut_down();
}

#[test]
fn entry_is_refused_while_a_turn_runs() {
    let mut p = Plan::new("plan-entry-busy", true, SessionModeChoice::Auto);
    p.queue(vec![AgentDomainEvent::TurnStarted { turn_id: "t1".into() }]);
    p.pump();
    let refused = p.set.set_planning(p.tab, ViewId::BASE, true, 0).unwrap_err();
    assert!(refused.benign);
    assert_eq!(refused.message, "plan mode starts when the tab is idle");
    assert!(p.provider.planning_calls().is_empty());
    p.shut_down();
}

#[test]
fn an_empty_tab_starts_in_plan_before_its_first_turn() {
    let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    let tab = set.active();
    assert!(set.set_planning(tab, ViewId::BASE, true, 0).unwrap().is_none());
    assert!(set.get(tab).unwrap().plan_on_start());
    assert!(set.set_planning(tab, ViewId::BASE, false, 0).unwrap().is_none());
    assert!(!set.get(tab).unwrap().plan_on_start());
    // Plan mode is a Claude sidecar's.
    let mut legacy = TabSet::new(BackendKind::Legacy, SessionModeChoice::Auto);
    let tab = legacy.active();
    assert!(legacy.set_planning(tab, ViewId::BASE, true, 0).is_err());
}

fn first_turn(text: &str) -> FirstTurn {
    FirstTurn {
        wire: text.into(),
        typed: text.into(),
        typed_by_person: true,
        shell_line: false,
        attachments: Vec::new(),
    }
}

/// A tab asked to start in plan mode (an empty tab's `<leader>p`, or a tab restored while it planned): its
/// first message waits until the CLI reports plan mode, then goes.
#[test]
fn a_held_first_turn_is_released_only_after_planning_is_confirmed() {
    for restored in [false, true] {
        let mut p = Plan::new("plan-first-turn", true, SessionModeChoice::Auto);
        p.set.get_mut(p.tab).unwrap().plan_on_start = true;
        let sent = if restored {
            p.set
                .send_prompt(p.tab, "r1", ViewId::BASE, first_turn("plan the refactor"))
        } else {
            p.set.send_first_turn(p.tab, "r1", first_turn("plan the refactor"))
        };
        assert!(matches!(
            sent,
            Some(Sent::First { outcome: Ok(()), .. } | Sent::Prompt { outcome: Ok(()), .. })
        ));
        assert_eq!(p.provider.planning_calls(), vec![true]);
        assert!(p.provider.turns().is_empty(), "held until the CLI is in plan mode");
        p.pump();
        assert!(p.set.release_after_card().is_empty());
        assert!(p.provider.turns().is_empty());
        p.queue(vec![AgentDomainEvent::CliPlanMode {
            source: "CliModeReported".into(),
        }]);
        p.pump_until("planning", |pr| pr.planning.phase == PlanPhase::Planning);
        assert!(!p.tab().plan_on_start());
        let released = p.set.release_after_card();
        assert_eq!(released.len(), 1, "restored {restored}");
        assert_eq!(p.provider.turns().len(), 1);
        p.shut_down();
    }
}

#[test]
fn leaving_lands_in_manual() {
    let mut p = Plan::new("plan-leave", true, SessionModeChoice::Auto);
    p.enter();
    p.queue(vec![AgentDomainEvent::TurnCompleted {
        turn_id: "t1".into(),
        outcome: agent::TurnOutcome::Completed,
        result_text: String::new(),
        stop_reason: None,
        usage: None,
        detail: Default::default(),
    }]);
    p.pump();
    let switch = p
        .set
        .set_planning(p.tab, ViewId::BASE, false, wall_clock_ms())
        .unwrap()
        .unwrap();
    assert!(!switch.enter());
    assert_eq!(p.set.finish_planning_switch(p.tab, &switch).unwrap().ok(), Some(()));
    assert!(matches!(p.phase(), PlanPhase::Leaving { .. }));
    p.cli_back("auto");
    assert_eq!(p.tab().mode(), SessionModeChoice::Manual);
    assert_eq!(p.tab().plan_origin(), None);
    p.shut_down();
}

#[test]
fn shift_tab_while_planning_is_refused_with_the_leave_hint() {
    let mut p = Plan::new("plan-shift-tab", true, SessionModeChoice::Auto);
    p.enter();
    assert_eq!(p.set.cycle_mode(p.tab, ViewId::BASE).unwrap_err(), PLAN_CYCLE_REFUSAL);
    assert_eq!(p.tab().mode(), SessionModeChoice::Auto);
    p.shut_down();
}

/// The person approves with Bypass offered and presses Shift+Tab at once: refused until the CLI confirms,
/// then the tab is in the mode the approval named and cycles from there.
#[test]
fn shift_tab_during_leaving_is_refused_until_the_cli_confirms() {
    let mut p = Plan::new("plan-shift-tab-leaving", true, SessionModeChoice::Bypass);
    p.enter();
    p.propose();
    let mut send = p
        .set
        .approve_plan(
            p.tab,
            ViewId::BASE,
            "p1",
            PlanChoice::ApproveBypass,
            SHA,
            wall_clock_ms(),
        )
        .unwrap();
    assert_eq!(
        p.set.finish_plan_send(p.tab, &mut send, wall_clock_ms()).unwrap().ok(),
        Some(())
    );
    assert!(matches!(p.phase(), PlanPhase::Leaving { .. }));
    assert_eq!(
        p.set.cycle_mode(p.tab, ViewId::BASE).unwrap_err(),
        PLAN_CHANGING_REFUSAL
    );
    p.settle(PlanOutcome::Approved, Some(PlanExecutionMode::Bypass));
    assert_eq!(
        p.set.cycle_mode(p.tab, ViewId::BASE).unwrap_err(),
        PLAN_CHANGING_REFUSAL
    );
    p.cli_back("auto");
    assert_eq!(p.tab().mode(), SessionModeChoice::Bypass);
    assert_eq!(
        p.set.cycle_mode(p.tab, ViewId::BASE).unwrap(),
        ModeCycle::Changed(SessionModeChoice::Manual)
    );
    p.shut_down();
}

#[test]
fn approve_bypass_needs_a_bypass_origin() {
    let mut p = Plan::new("plan-approve-bypass", true, SessionModeChoice::Auto);
    p.enter();
    p.propose();
    let refused = p
        .set
        .approve_plan(p.tab, ViewId::BASE, "p1", PlanChoice::ApproveBypass, SHA, 0)
        .unwrap_err();
    assert_eq!(
        refused.message,
        "Approve and keep Bypass is offered only when plan mode started in Bypass"
    );
    assert!(p.provider.interactions().is_empty());
    assert_eq!(
        p.backend().projection().waiting_plan().unwrap().state,
        PlanState::Waiting
    );
    p.shut_down();
}

/// `[Bash, ExitPlanMode]` in one message: the approval waits until the deferred `Bash` has finished.
#[test]
fn approval_waits_for_a_deferred_call_to_finish() {
    let mut p = Plan::new("plan-drain", true, SessionModeChoice::Auto);
    p.enter();
    p.queue(vec![AgentDomainEvent::ToolCallStarted {
        turn_id: "t1".into(),
        tool_use_id: "toolu_b".into(),
        name: "Bash".into(),
        input: bash("ls"),
    }]);
    p.ask("b", "Bash", bash("ls"), false);
    assert_eq!(p.decision("b"), Some(PermissionDecision::Defer));
    p.propose();
    let t0 = wall_clock_ms();
    let mut send = p
        .set
        .approve_plan(p.tab, ViewId::BASE, "p1", PlanChoice::ApproveAuto, SHA, t0)
        .unwrap();
    assert_eq!(send.waiting_calls(), 1);
    assert!(p.set.finish_plan_send(p.tab, &mut send, t0 + 1_000).is_none());
    assert!(p.provider.interactions().is_empty(), "nothing goes while the call runs");
    p.queue(vec![AgentDomainEvent::ToolCallCompleted {
        turn_id: "t1".into(),
        tool_use_id: "toolu_b".into(),
        content: serde_json::json!("a.txt"),
        is_error: false,
    }]);
    p.pump();
    assert_eq!(
        p.set.finish_plan_send(p.tab, &mut send, t0 + 2_000).unwrap().ok(),
        Some(())
    );
    assert_eq!(p.provider.interactions().len(), 1);
    assert_eq!(send.waiting_calls(), 0);
    p.shut_down();
}

#[test]
fn a_drain_past_15_s_refuses_the_approval_and_keeps_the_plan() {
    let mut p = Plan::new("plan-drain-late", true, SessionModeChoice::Auto);
    p.enter();
    p.ask("w", "Write", write(), false);
    p.propose();
    let t0 = wall_clock_ms();
    let mut send = p
        .set
        .approve_plan(p.tab, ViewId::BASE, "p1", PlanChoice::ApproveManual, SHA, t0)
        .unwrap();
    assert!(p.set.finish_plan_send(p.tab, &mut send, t0 + 15_000).is_none());
    let refused = p
        .set
        .finish_plan_send(p.tab, &mut send, t0 + 15_001)
        .unwrap()
        .unwrap_err();
    assert!(refused.benign, "{}", refused.message);
    assert!(
        refused.message.contains("the plan is still waiting"),
        "{}",
        refused.message
    );
    assert!(p.provider.interactions().is_empty());
    assert_eq!(
        p.backend().projection().waiting_plan().unwrap().state,
        PlanState::Waiting
    );
    assert_eq!(p.phase(), PlanPhase::Planning);
    p.shut_down();
}

#[test]
fn an_approved_settlement_sets_the_mode_from_mode_after_even_without_the_local_reply() {
    let mut p = Plan::new("plan-settled-elsewhere", true, SessionModeChoice::Auto);
    p.enter();
    p.propose();
    // Another window approved it into Manual.
    p.settle(PlanOutcome::Approved, Some(PlanExecutionMode::Manual));
    p.cli_back("auto");
    assert_eq!(p.tab().mode(), SessionModeChoice::Manual);
    assert_eq!(p.tab().plan_origin(), None);
    p.shut_down();
}

#[test]
fn an_approval_with_unknown_outcome_drops_to_manual_not_the_origin() {
    let mut p = Plan::new("plan-unknown", true, SessionModeChoice::Bypass);
    p.enter();
    p.propose();
    p.provider.hold_interaction_replies(true);
    let mut send = p
        .set
        .approve_plan(p.tab, ViewId::BASE, "p1", PlanChoice::ApproveBypass, SHA, 0)
        .unwrap();
    assert!(p.set.finish_plan_send(p.tab, &mut send, 0).is_none());
    p.provider
        .reply_to_held_interactions(|| Err(agent::ProviderError::Timeout));
    assert!(p.set.finish_plan_send(p.tab, &mut send, 0).unwrap().is_err());
    assert_eq!(p.tab().mode(), SessionModeChoice::Manual);
    p.shut_down();
}

#[test]
fn a_late_settlement_after_an_unknown_outcome_sets_its_mode() {
    let mut p = Plan::new("plan-late-settlement", true, SessionModeChoice::Manual);
    p.enter();
    p.propose();
    p.provider.hold_interaction_replies(true);
    let mut send = p
        .set
        .approve_plan(p.tab, ViewId::BASE, "p1", PlanChoice::ApproveAuto, SHA, 0)
        .unwrap();
    p.provider
        .reply_to_held_interactions(|| Err(agent::ProviderError::Timeout));
    assert!(p.set.finish_plan_send(p.tab, &mut send, 0).unwrap().is_err());
    assert_eq!(p.tab().mode(), SessionModeChoice::Manual);
    p.settle(PlanOutcome::Approved, Some(PlanExecutionMode::Auto));
    p.cli_back("auto");
    assert_eq!(p.tab().mode(), SessionModeChoice::Auto);
    p.shut_down();
}

/// Approved out of a Bypass round into Manual: a request that arrives while the session leaves plan mode,
/// with the tab still in Bypass, is answered by nobody and drawn as no card, and judged once the tab is in
/// Manual: a card, never Bypass's allow.
#[test]
fn held_requests_are_answered_once_the_cli_confirms_the_new_mode() {
    let mut p = Plan::new("plan-held", true, SessionModeChoice::Bypass);
    p.enter();
    p.propose();
    let mut send = p
        .set
        .approve_plan(
            p.tab,
            ViewId::BASE,
            "p1",
            PlanChoice::ApproveManual,
            SHA,
            wall_clock_ms(),
        )
        .unwrap();
    assert!(p
        .set
        .finish_plan_send(p.tab, &mut send, wall_clock_ms())
        .unwrap()
        .is_ok());
    // The CLI reported its way out (`default` on an auto session is a step it may correct); the
    // settlement naming Manual has not arrived, so the tab is still in Bypass.
    p.queue(vec![AgentDomainEvent::CliPermissionMode {
        reported: "default".into(),
    }]);
    p.pump();
    assert!(matches!(p.phase(), PlanPhase::Leaving { .. }));
    assert_eq!(p.tab().mode(), SessionModeChoice::Bypass);
    p.ask("w", "Write", write(), false);
    p.ask("r", "Read", serde_json::json!({ "file_path": "main.rs" }), false);
    assert_eq!(p.decision("w"), None);
    assert_eq!(p.decision("r"), None);
    assert!(!p.carded("w") && !p.carded("r"), "held, drawn as no card");
    p.settle(PlanOutcome::Approved, Some(PlanExecutionMode::Manual));
    p.cli_back("auto");
    p.pump();
    p.pump();
    assert_eq!(p.tab().mode(), SessionModeChoice::Manual);
    assert_eq!(p.decision("w"), None, "Manual cards every write");
    assert!(p.carded("w"));
    assert_eq!(
        p.decision("r"),
        Some(PermissionDecision::Allow),
        "answered once, under Manual"
    );
    p.shut_down();
}

#[test]
fn an_overdue_transition_fails_the_tab() {
    let mut p = Plan::new("plan-overdue", true, SessionModeChoice::Auto);
    let now = wall_clock_ms();
    let switch = p.set.set_planning(p.tab, ViewId::BASE, true, now).unwrap().unwrap();
    assert!(
        p.set.check_plan_transitions(now + 60_000).is_empty(),
        "its reply is unread"
    );
    assert_eq!(p.set.finish_planning_switch(p.tab, &switch).unwrap().ok(), Some(()));
    assert!(p.set.check_plan_transitions(now + 1_000).is_empty());
    let tripped = p.set.check_plan_transitions(wall_clock_ms() + 16_000);
    assert_eq!(tripped.len(), 1);
    assert!(
        tripped[0].reason.contains("did not enter plan mode"),
        "{}",
        tripped[0].reason
    );
    assert!(matches!(p.tab().backend(), TabBackend::Failed { .. }));
    for tripped in tripped {
        tripped.retired.into_backend().shutdown();
    }
    p.shut_down();
}

#[test]
fn bypass_approve_pending_confirm_bypass_and_cycle_never_settle_a_plan() {
    let mut p = Plan::new("plan-sweeps", true, SessionModeChoice::Auto);
    p.enter();
    p.propose();
    assert_eq!(p.set.cycle_mode(p.tab, ViewId::BASE).unwrap_err(), PLAN_CYCLE_REFUSAL);
    let answered = p
        .set
        .get_mut(p.tab)
        .unwrap()
        .live_mut()
        .unwrap()
        .approve_pending(&["p1".to_string()], "on entering bypass");
    assert!(answered.ids.is_empty());
    assert!(p.provider.interactions().is_empty());
    assert!(p.provider.decisions().is_empty());
    assert_eq!(
        p.backend().projection().waiting_plan().unwrap().state,
        PlanState::Waiting
    );
    assert!(p.set.close_needs_confirm(p.tab));
    assert!(tabs::close_prompt(&p.set.close_facts(p.tab).unwrap())
        .contains(&"closing ends Claude's plan without a decision".to_string()));
    p.shut_down();
}

/// A batch so long it overflows the page's queue, ending in `last`: the tab resyncs and draws what the batch
/// left pending from a snapshot. Returns once that snapshot went out.
fn resync_with(p: &mut Plan, last: Vec<AgentDomainEvent>) {
    let turn = p
        .backend()
        .projection()
        .active_turn_id
        .clone()
        .unwrap_or_else(|| "t1".into());
    let mut overflow = Vec::new();
    for i in 0..300 {
        overflow.push(AgentDomainEvent::ContentDelta {
            turn_id: turn.clone(),
            kind: agent::ContentKind::Text,
            text: format!("chunk {i}"),
        });
    }
    overflow.extend(last);
    p.provider.queue_all(overflow);
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        assert!(Instant::now() < deadline, "timed out waiting for the resync");
        let out = p.set.pump(&p.dir, true);
        if let Some(payload) = out.active_payload {
            let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
            if value["kind"] == "snapshot" {
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A card raised in plan mode stays a person's after plan mode ends in Bypass, the CLI's prompt arriving only
/// through a resync: a later bypass resync and the confirmation never approve it, and a person's own allow goes
/// through.
#[test]
fn a_card_raised_in_plan_is_only_ever_answered_by_a_person() {
    let mut p = Plan::new("plan-card-later", true, SessionModeChoice::Bypass);
    p.enter();
    resync_with(&mut p, vec![request("w", "Write", write(), true)]);
    assert!(p.carded("w"));
    p.propose();
    p.settle(PlanOutcome::Approved, Some(PlanExecutionMode::Bypass));
    p.cli_back("auto");
    p.pump();
    assert_eq!(p.tab().mode(), SessionModeChoice::Bypass);
    assert_eq!(p.phase(), PlanPhase::Executing);
    // A resync in Bypass sweeps what it finds pending, but not this card.
    resync_with(&mut p, Vec::new());
    p.pump();
    assert_eq!(
        p.decision("w"),
        None,
        "the bypass resync took a card raised in plan mode"
    );
    assert!(waiting_cards(p.tab()).is_empty());
    p.set
        .answer_card(p.tab, ViewId::BASE, "w", PermissionDecision::Allow)
        .unwrap();
    assert_eq!(p.decision("w"), Some(PermissionDecision::Allow));
    p.shut_down();
}

#[test]
fn the_editor_broker_sees_auto_in_a_bypass_origin_plan() {
    let mut p = Plan::new("plan-broker", true, SessionModeChoice::Bypass);
    p.provider.open_session("claude-1", &p.dir);
    p.pump();
    let before = p.set.broker_facts(p.tab).map(|facts| facts.mode);
    p.enter();
    let during = p.set.broker_facts(p.tab).map(|facts| facts.mode);
    assert_eq!(before, Some(PermissionMode::Bypass), "the broker has the tab's facts");
    assert_eq!(during, Some(PermissionMode::Auto));
    p.shut_down();
}

/// A tab saved while planning comes back marked to ask for plan mode before its first message (and in its
/// execution mode underneath); one saved executing does not.
#[test]
fn a_restored_plan_tab_reenters_plan_before_its_first_input() {
    let planned = |id: &str, plan_origin| PlannedTab {
        agent: AgentId::Claude,
        provider_session_id: id.into(),
        name: None,
        title: Some(id.into()),
        saved_mode: SessionModeChoice::Auto,
        label: id.into(),
        view: None,
        turn_running: false,
        draft: None,
        row_key: None,
        plan_origin,
    };
    let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    let from = set.active();
    let plan = RestorePlan {
        items: vec![
            planned("planning", Some(SessionModeChoice::Auto)),
            planned("executing", None),
        ],
        skipped: Vec::new(),
        active_session: Some("planning".into()),
    };
    let RestoreStep::Go(go) = set.begin_restore(plan, BypassPolicy::Ask) else {
        panic!("nothing in bypass, nothing to ask");
    };
    set.start_restore(from, go, |_, _| {
        Ok(RestoreConnect::Started(std::sync::mpsc::channel().1))
    });
    let resumed = |tab: &Tab| match &tab.backend {
        TabBackend::Starting(start) => start.resume.clone(),
        _ => None,
    };
    let restored: Vec<(Option<String>, bool, SessionModeChoice)> = set
        .tabs
        .iter()
        .filter(|tab| resumed(tab).is_some())
        .map(|tab| (resumed(tab), tab.plan_on_start(), tab.mode()))
        .collect();
    assert_eq!(restored.len(), 2, "{restored:?}");
    for (id, plan_on_start, mode) in restored {
        assert_eq!(plan_on_start, id.as_deref() == Some("planning"), "{id:?}");
        assert_eq!(mode, SessionModeChoice::Auto, "{id:?}");
    }
}

/// A Bypass tab the person moves into plan mode: until the CLI's report of plan arrives, its own prompt for a
/// call already held back in plan mode is still a card, never Bypass's allow.
#[test]
fn a_provider_prompt_while_entering_is_a_card_even_in_bypass() {
    let mut p = Plan::new("plan-entering-prompt", true, SessionModeChoice::Bypass);
    p.set
        .set_planning(p.tab, ViewId::BASE, true, wall_clock_ms())
        .unwrap()
        .expect("a live session sends the switch");
    assert!(matches!(p.phase(), PlanPhase::Entering { .. }));
    p.queue(vec![AgentDomainEvent::TurnStarted { turn_id: "t1".into() }]);
    p.pump();
    p.ask(
        "w",
        "Write",
        serde_json::json!({ "file_path": "src/x.txt", "content": "x" }),
        true,
    );
    assert_eq!(p.decision("w"), None, "Bypass allowed the CLI's plan-mode prompt");
    assert!(p.carded("w"));
    p.queue(vec![AgentDomainEvent::CliPlanMode {
        source: "CliModeReported".into(),
    }]);
    p.pump_until("planning", |p| p.planning.phase == PlanPhase::Planning);
    assert!(
        waiting_cards(p.tab()).is_empty(),
        "a card raised while entering is a person's"
    );
    p.shut_down();
}

/// While planning, a gate request for `Edit` or `NotebookEdit` (nobody saw the CLI ask before either), and one
/// for any of the four tools that cannot be tied to the main thread (a subagent's call, a call not seen), is a
/// card in every origin, never left to a plan mode the call may not run under.
#[test]
fn unmeasured_and_untied_calls_in_plan_are_cards() {
    for origin in ORIGINS {
        let mut p = Plan::new("plan-untied", true, origin);
        p.enter();
        p.ask(
            "e",
            "Edit",
            serde_json::json!({ "file_path": "main.rs", "old_string": "a", "new_string": "b" }),
            false,
        );
        p.ask(
            "n",
            "NotebookEdit",
            serde_json::json!({ "notebook_path": "a.ipynb", "new_source": "" }),
            false,
        );
        p.queue(vec![
            AgentDomainEvent::ToolCallStarted {
                turn_id: "t1".into(),
                tool_use_id: "toolu_task".into(),
                name: "Task".into(),
                input: serde_json::json!({ "prompt": "write it" }),
            },
            AgentDomainEvent::SubagentFrame {
                parent_tool_use_id: "toolu_task".into(),
                frame: Box::new(AgentDomainEvent::ToolCallStarted {
                    turn_id: "t1".into(),
                    tool_use_id: "toolu_sw".into(),
                    name: "Write".into(),
                    input: write(),
                }),
            },
            request("sw", "Write", write(), false),
            // No call of this id was seen: judged once the wait for it is over.
            request("x", "Bash", bash("touch y"), false),
        ]);
        p.pump();
        p.pump();
        p.wait_out();
        assert!(
            !p.backend()
                .projection()
                .tool_calls
                .iter()
                .any(|c| c.tool_use_id == "toolu_sw"),
            "the premise: a subagent's call is not the main thread's"
        );
        for id in ["e", "n", "sw", "x"] {
            assert_eq!(p.decision(id), None, "{origin:?} {id}");
            assert!(p.carded(id), "{origin:?} {id}");
        }
        p.shut_down();
    }
}

/// An approval does not wait for a person: while a call deferred in plan mode has a card up, it is refused with
/// words at once (and a card that comes up while it waits refuses it then), never counted down to the guard.
#[test]
fn an_approval_while_a_deferred_calls_card_waits_is_refused_with_words() {
    let prompt = |id: &str| AgentDomainEvent::PermissionRequested {
        permission_id: id.into(),
        tool_use_id: Some("toolu_b".into()),
        tool_name: "Bash".into(),
        input: bash("touch y"),
        provider_prompt: Some(agent::ProviderPrompt {
            reason: Some("plan mode".into()),
            description: None,
            blocked_path: None,
            matched_ask_rule: None,
            unrecognized_origin: None,
        }),
    };
    // Up front.
    let mut p = Plan::new("plan-drain-card", true, SessionModeChoice::Auto);
    p.enter();
    p.ask("b", "Bash", bash("touch y"), false);
    assert_eq!(p.decision("b"), Some(PermissionDecision::Defer));
    p.queue(vec![prompt("b2")]);
    p.pump();
    p.pump();
    assert!(p.carded("b2"));
    p.propose();
    let refused = p
        .set
        .approve_plan(p.tab, ViewId::BASE, "p1", PlanChoice::ApproveAuto, SHA, wall_clock_ms())
        .unwrap_err();
    assert!(refused.benign);
    assert_eq!(refused.message, PLAN_CARD_FIRST);
    assert!(p.provider.interactions().is_empty());
    assert_eq!(
        p.backend().projection().waiting_plan().unwrap().state,
        PlanState::Waiting
    );
    p.shut_down();
    // While the approval waits.
    let mut p = Plan::new("plan-drain-card-late", true, SessionModeChoice::Auto);
    p.enter();
    p.ask("b", "Bash", bash("touch y"), false);
    p.propose();
    let t0 = wall_clock_ms();
    let mut send = p
        .set
        .approve_plan(p.tab, ViewId::BASE, "p1", PlanChoice::ApproveAuto, SHA, t0)
        .unwrap();
    assert_eq!(send.waiting_calls(), 1);
    p.queue(vec![prompt("b2")]);
    p.pump();
    p.pump();
    let refused = p
        .set
        .finish_plan_send(p.tab, &mut send, t0 + 1_000)
        .expect("refused at once, not at the guard")
        .unwrap_err();
    assert_eq!(refused.message, PLAN_CARD_FIRST);
    assert!(p.provider.interactions().is_empty());
    p.shut_down();
}

/// A base-page tab in the background has nobody looking at its plan: no approval of it and no switch of it goes,
/// whatever the page names; on screen again, it does.
#[test]
fn an_approval_or_a_switch_for_a_tab_not_on_screen_is_refused() {
    let mut p = Plan::new("plan-not-on-screen", true, SessionModeChoice::Bypass);
    p.enter();
    p.propose();
    let other = p.set.open(AgentId::Claude);
    assert_eq!(p.set.active(), other);
    let hidden = format!("tab {} is not the one on screen", p.tab().number);
    for choice in [
        PlanChoice::ApproveManual,
        PlanChoice::ApproveAuto,
        PlanChoice::ApproveBypass,
    ] {
        let refused = p
            .set
            .approve_plan(p.tab, ViewId::BASE, "p1", choice, SHA, wall_clock_ms())
            .unwrap_err();
        assert_eq!(refused.message, hidden, "{choice:?}");
    }
    for enter in [true, false] {
        let refused = p
            .set
            .set_planning(p.tab, ViewId::BASE, enter, wall_clock_ms())
            .unwrap_err();
        assert_eq!(refused.message, hidden);
    }
    assert!(p.provider.interactions().is_empty());
    assert_eq!(
        p.backend().projection().waiting_plan().unwrap().state,
        PlanState::Waiting
    );
    assert!(p.set.select(p.tab));
    let mut send = p
        .set
        .approve_plan(
            p.tab,
            ViewId::BASE,
            "p1",
            PlanChoice::ApproveManual,
            SHA,
            wall_clock_ms(),
        )
        .unwrap();
    assert_eq!(
        p.set.finish_plan_send(p.tab, &mut send, wall_clock_ms()).unwrap().ok(),
        Some(())
    );
    assert_eq!(p.provider.interactions().len(), 1);
    p.shut_down();
}

/// While an approval waits for a deferred call, a second approval is refused: the first would go out and the
/// second be refused later, so the choice the person made last would lose. Once the first went, the record
/// decides again.
#[test]
fn a_second_approval_while_the_first_drains_is_refused() {
    let mut p = Plan::new("plan-drain-twice", true, SessionModeChoice::Auto);
    p.enter();
    p.queue(vec![AgentDomainEvent::ToolCallStarted {
        turn_id: "t1".into(),
        tool_use_id: "toolu_b".into(),
        name: "Bash".into(),
        input: bash("ls"),
    }]);
    p.ask("b", "Bash", bash("ls"), false);
    p.propose();
    let t0 = wall_clock_ms();
    let mut first = p
        .set
        .approve_plan(p.tab, ViewId::BASE, "p1", PlanChoice::ApproveAuto, SHA, t0)
        .unwrap();
    assert_eq!(first.waiting_calls(), 1);
    let refused = p
        .set
        .approve_plan(p.tab, ViewId::BASE, "p1", PlanChoice::ApproveManual, SHA, t0)
        .unwrap_err();
    assert_eq!(refused.message, PLAN_APPROVAL_DRAINING);
    assert!(p.set.finish_plan_send(p.tab, &mut first, t0 + 1_000).is_none());
    p.queue(vec![AgentDomainEvent::ToolCallCompleted {
        turn_id: "t1".into(),
        tool_use_id: "toolu_b".into(),
        content: serde_json::json!("a.txt"),
        is_error: false,
    }]);
    p.pump();
    assert_eq!(
        p.set.finish_plan_send(p.tab, &mut first, t0 + 2_000).unwrap().ok(),
        Some(())
    );
    assert_eq!(p.tab().session.plan.draining, None);
    let interactions = p.provider.interactions();
    assert_eq!(interactions.len(), 1, "{interactions:?}");
    p.shut_down();
}

/// A drain refused at the guard lets the person approve again.
#[test]
fn a_drain_refused_at_the_guard_lets_another_approval_through() {
    let mut p = Plan::new("plan-drain-guard-again", true, SessionModeChoice::Auto);
    p.enter();
    p.queue(vec![AgentDomainEvent::ToolCallStarted {
        turn_id: "t1".into(),
        tool_use_id: "toolu_b".into(),
        name: "Bash".into(),
        input: bash("ls"),
    }]);
    p.ask("b", "Bash", bash("ls"), false);
    p.propose();
    let t0 = wall_clock_ms();
    let mut first = p
        .set
        .approve_plan(p.tab, ViewId::BASE, "p1", PlanChoice::ApproveAuto, SHA, t0)
        .unwrap();
    assert!(p
        .set
        .finish_plan_send(p.tab, &mut first, t0 + agent::PLAN_TRANSITION_GUARD_MS + 1)
        .unwrap()
        .is_err());
    let again = p
        .set
        .approve_plan(p.tab, ViewId::BASE, "p1", PlanChoice::ApproveManual, SHA, t0)
        .unwrap();
    assert_eq!(again.waiting_calls(), 1);
    p.shut_down();
}

impl Plan {
    /// The CLI's request for the main thread's call `toolu_<id>` arrives before the call's own start, as the sidecar
    /// sends them.
    fn ask_ahead(&mut self, id: &str, tool: &str, input: serde_json::Value, prompt: bool) {
        self.queue(vec![request(id, tool, input, prompt)]);
        self.pump();
        self.pump();
    }

    /// The call `toolu_<id>` starts in the running turn.
    fn start(&mut self, id: &str, tool: &str, input: serde_json::Value) {
        self.queue(vec![AgentDomainEvent::ToolCallStarted {
            turn_id: "t1".into(),
            tool_use_id: format!("toolu_{id}"),
            name: tool.into(),
            input,
        }]);
        self.pump();
        self.pump();
    }

    fn waits(&self, id: &str) -> bool {
        self.tab().session.plan.unsequenced.contains_key(id)
    }

    /// Every request waiting for its call has waited as long as it may.
    fn wait_out(&mut self) {
        let waiting = &mut self.set.get_mut(self.tab).unwrap().session.plan.unsequenced;
        for since in waiting.values_mut() {
            *since = since.saturating_sub(crate::agent_backend::PLAN_SEQUENCE_WAIT_MS + 1);
        }
        self.pump();
        self.pump();
    }

    fn no_allow(&self) {
        assert!(
            self.provider.decisions().iter().all(|(_, d)| !d.allows()),
            "{:?}",
            self.provider.decisions()
        );
    }
}

/// The model enters plan mode in a Bypass tab and then writes and runs: the hook requests arrive before the CLI's
/// `plan` report and before their calls. Nothing is answered until each call has been folded; then the plan table
/// answers (the main thread's `Write` and `Bash` are left to the CLI's plan mode), and the CLI's own prompts for
/// them are cards a person alone answers. Bypass allows none of them.
#[test]
fn bypass_requests_ahead_of_their_calls_wait_and_follow_the_plan_table() {
    let mut p = Plan::new("plan-ahead-bypass", true, SessionModeChoice::Bypass);
    p.queue(vec![AgentDomainEvent::TurnStarted { turn_id: "t1".into() }]);
    p.pump();
    p.ask_ahead("w", "Write", write(), false);
    p.ask_ahead("b", "Bash", bash("touch x"), false);
    for id in ["w", "b"] {
        assert_eq!(p.decision(id), None, "{id} is not judged before its call");
        assert!(!p.carded(id), "{id} is drawn as no card while it waits");
        assert!(p.waits(id), "{id}");
    }
    p.queue(vec![
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
    p.start("w", "Write", write());
    p.start("b", "Bash", bash("touch x"));
    assert_eq!(p.decision("w"), Some(PermissionDecision::Defer));
    assert_eq!(p.decision("b"), Some(PermissionDecision::Defer));
    assert!(!p.waits("w") && !p.waits("b"));
    // The CLI asks itself, as Verdandi turns plan mode's answer into the CLI's ask.
    p.queue(vec![
        prompt_on("w2", "toolu_w", "Write", write()),
        prompt_on("b2", "toolu_b", "Bash", bash("touch x")),
    ]);
    p.pump();
    p.pump();
    for id in ["w2", "b2"] {
        assert_eq!(p.decision(id), None, "{id}");
        assert!(p.carded(id), "{id}");
        assert!(p.tab().session.plan.cards.contains(id), "{id} is a person's alone");
    }
    p.no_allow();
    p.shut_down();
}

/// A Manual tab's card for a call the CLI made in plan mode, approved by the person: the CLI's own prompt for the same
/// call is a card too, never allowed with that approval.
#[test]
fn a_cli_prompt_in_plan_mode_is_never_allowed_with_an_earlier_approval() {
    let mut p = Plan::new("plan-ahead-approval", false, SessionModeChoice::Manual);
    p.queue(vec![AgentDomainEvent::TurnStarted { turn_id: "t1".into() }]);
    p.pump();
    let edit = serde_json::json!({ "file_path": "main.rs", "old_string": "a", "new_string": "b" });
    p.ask_ahead("e", "Edit", edit.clone(), false);
    assert!(!p.carded("e"), "not drawn before its call");
    p.queue(vec![
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
    p.start("e", "Edit", edit.clone());
    assert!(p.carded("e"));
    assert!(p.tab().session.plan.cards.contains("e"));
    p.set
        .answer_card(p.tab, ViewId::BASE, "e", PermissionDecision::Allow)
        .unwrap();
    p.queue(vec![prompt_on("e2", "toolu_e", "Edit", edit)]);
    p.pump();
    p.pump();
    assert_eq!(
        p.decision("e2"),
        None,
        "the CLI's own prompt in plan mode is the person's"
    );
    assert!(p.carded("e2"));
    p.shut_down();
}

/// A call beside `EnterPlanMode` began while the session was entering plan mode: the CLI may already plan. Where the
/// tab's mode and the plan table disagree it is a card, in Bypass too, and it is a person's alone.
#[test]
fn a_call_made_while_entering_is_a_card_where_the_two_verdicts_disagree() {
    let mut p = Plan::new("plan-ahead-entering", true, SessionModeChoice::Bypass);
    p.queue(vec![AgentDomainEvent::TurnStarted { turn_id: "t1".into() }]);
    p.pump();
    p.ask_ahead("b", "Bash", bash("touch y"), false);
    p.queue(vec![AgentDomainEvent::ToolCallStarted {
        turn_id: "t1".into(),
        tool_use_id: "toolu_enter".into(),
        name: "EnterPlanMode".into(),
        input: serde_json::json!({}),
    }]);
    p.start("b", "Bash", bash("touch y"));
    assert!(matches!(p.phase(), PlanPhase::Entering { .. }));
    assert_eq!(p.decision("b"), None);
    assert!(p.carded("b"));
    assert!(p.tab().session.plan.cards.contains("b"));
    p.no_allow();
    p.shut_down();
}

/// A request whose call the sidecar never shows (a workflow's own call) is judged once the wait is over, in both modes:
/// a card while the session plans and a card while it executes. Nothing folded after the request says the CLI was not
/// planning when it asked: another call's start and a turn's end may each have been sent before the request, which
/// overtakes them, and a turn's end does not end the wait.
#[test]
fn a_request_whose_call_never_comes_is_judged_after_the_wait() {
    let mut p = Plan::new("plan-ahead-never", true, SessionModeChoice::Bypass);
    p.enter();
    p.ask_ahead("x", "Write", write(), false);
    assert!(p.waits("x"));
    assert_eq!(p.decision("x"), None);
    p.wait_out();
    assert!(!p.waits("x"));
    assert_eq!(p.decision("x"), None, "planning: a card");
    assert!(p.carded("x"));
    assert!(p.tab().session.plan.cards.contains("x"));
    p.shut_down();

    // Nothing folded after the request: the message path may only lag, a `plan` report still queued in it.
    let mut q = Plan::new("plan-ahead-never-exec", true, SessionModeChoice::Bypass);
    q.queue(vec![AgentDomainEvent::TurnStarted { turn_id: "t1".into() }]);
    q.pump();
    q.ask_ahead("y", "Write", write(), false);
    assert!(q.waits("y"));
    assert_eq!(q.decision("y"), None);
    q.wait_out();
    assert!(!q.waits("y"));
    assert_eq!(q.decision("y"), None, "executing is not known: a card");
    assert!(q.carded("y"));
    assert!(q.tab().session.plan.cards.contains("y"), "a person's alone");
    q.no_allow();
    q.shut_down();

    // Another call began after the request with no plan mode reported: that call may have been sent first.
    let mut s = Plan::new("plan-ahead-never-passed", true, SessionModeChoice::Bypass);
    s.queue(vec![AgentDomainEvent::TurnStarted { turn_id: "t1".into() }]);
    s.pump();
    s.ask_ahead("v", "Write", write(), false);
    s.start("other", "Read", serde_json::json!({ "file_path": "main.rs" }));
    assert!(s.waits("v"), "its own call is still unseen");
    s.wait_out();
    assert_eq!(s.decision("v"), None, "a later call is no evidence: a card");
    assert!(s.carded("v"));
    s.no_allow();
    s.shut_down();

    // A turn that ends after the request neither ends the wait nor places it.
    let mut r = Plan::new("plan-ahead-turn-end", true, SessionModeChoice::Bypass);
    r.queue(vec![AgentDomainEvent::TurnStarted { turn_id: "t1".into() }]);
    r.pump();
    r.ask_ahead("z", "Write", write(), false);
    assert!(r.waits("z"));
    r.queue(vec![turn_done()]);
    r.pump();
    r.pump();
    assert!(r.waits("z"), "a turn's end does not end the wait");
    assert_eq!(r.decision("z"), None);
    r.wait_out();
    assert_eq!(r.decision("z"), None, "a turn's end is no evidence: a card");
    assert!(r.carded("z"));
    r.no_allow();
    r.shut_down();
}

/// The order the sidecar can deliver (`c40_fast`'s recording has it): the plan-mode `Write` request, then the start of
/// an unrelated call sent before it (`ToolSearch`), then nothing more until the wait is over, `EnterPlanMode` and the
/// CLI's `plan` report still on their way. The `Write` is a card, in an Auto tab whose CLI runs default (whose own
/// verdict would allow a write to a project file) and in Bypass, and stays one once the report folds.
#[test]
fn an_earlier_call_folded_after_a_request_does_not_place_it() {
    for (origin, cli_auto) in [(SessionModeChoice::Auto, false), (SessionModeChoice::Bypass, true)] {
        let mut p = Plan::new("plan-overtaken", cli_auto, origin);
        p.queue(vec![AgentDomainEvent::TurnStarted { turn_id: "t1".into() }]);
        p.pump();
        let file = serde_json::json!({ "file_path": "main.rs", "content": "fn main() { }" });
        p.ask_ahead("w", "Write", file.clone(), false);
        p.start("ts", "ToolSearch", serde_json::json!({ "query": "select:Write" }));
        assert!(p.waits("w"), "{origin:?}: an earlier call's start does not place it");
        assert_eq!(p.decision("w"), None, "{origin:?}");
        p.wait_out();
        assert_eq!(p.decision("w"), None, "{origin:?}: a card");
        assert!(p.carded("w"), "{origin:?}");
        assert!(p.tab().session.plan.cards.contains("w"), "{origin:?}: a person's alone");
        p.queue(vec![
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
        p.start("w", "Write", file);
        assert_eq!(p.phase(), PlanPhase::Planning);
        assert_eq!(p.decision("w"), None, "{origin:?}");
        p.no_allow();
        p.shut_down();
    }
}

/// A request that names no call is never placed: it waits the whole wait, whatever folds after it, and is then judged
/// in both modes. In Bypass the CLI's own prompt is a card: when a call begins after it with no plan mode reported,
/// when a background subagent's call folds after it in a pump of its own before `EnterPlanMode`'s start, when the CLI
/// reported plan mode first, and when nothing folds after it at all.
#[test]
fn a_request_naming_no_call_waits_the_whole_wait_and_is_a_card() {
    let unnamed = |id: &str| match request(id, "Bash", bash("touch x"), true) {
        AgentDomainEvent::PermissionRequested {
            permission_id,
            tool_name,
            input,
            provider_prompt,
            ..
        } => AgentDomainEvent::PermissionRequested {
            permission_id,
            tool_use_id: None,
            tool_name,
            input,
            provider_prompt,
        },
        _ => unreachable!("a request"),
    };
    let enter = || {
        vec![
            AgentDomainEvent::ToolCallStarted {
                turn_id: "t1".into(),
                tool_use_id: "toolu_enter".into(),
                name: "EnterPlanMode".into(),
                input: serde_json::json!({}),
            },
            AgentDomainEvent::CliPlanMode {
                source: "CliModeReported".into(),
            },
        ]
    };
    // A background subagent runs; its call, sent just before `EnterPlanMode`, folds after the request in a pump of its
    // own, and the entry and the report fold in the next.
    let mut b = Plan::new("plan-unnamed-subagent", true, SessionModeChoice::Bypass);
    b.queue(vec![
        AgentDomainEvent::TurnStarted { turn_id: "t1".into() },
        AgentDomainEvent::ToolCallStarted {
            turn_id: "t1".into(),
            tool_use_id: "toolu_task".into(),
            name: "Task".into(),
            input: serde_json::json!({ "prompt": "look around", "run_in_background": true }),
        },
    ]);
    b.pump();
    b.queue(vec![unnamed("u")]);
    b.pump();
    b.pump();
    assert!(b.waits("u"));
    b.queue(vec![AgentDomainEvent::SubagentFrame {
        parent_tool_use_id: "toolu_task".into(),
        frame: Box::new(AgentDomainEvent::ToolCallStarted {
            turn_id: "t1".into(),
            tool_use_id: "toolu_sub".into(),
            name: "Grep".into(),
            input: serde_json::json!({ "pattern": "x" }),
        }),
    }]);
    b.pump();
    b.pump();
    assert!(b.waits("u"), "a subagent's call does not place it");
    assert_eq!(b.decision("u"), None, "the subagent's call let the prompt through");
    b.queue(enter());
    b.pump();
    b.pump();
    assert_eq!(b.phase(), PlanPhase::Planning);
    b.wait_out();
    assert_eq!(b.decision("u"), None);
    assert!(b.carded("u"));
    assert!(b.tab().session.plan.cards.contains("u"));
    b.no_allow();
    b.shut_down();

    let mut p = Plan::new("plan-unnamed-exec", true, SessionModeChoice::Bypass);
    p.queue(vec![AgentDomainEvent::TurnStarted { turn_id: "t1".into() }]);
    p.pump();
    p.queue(vec![unnamed("u")]);
    p.pump();
    p.pump();
    assert!(p.waits("u"));
    assert_eq!(p.decision("u"), None);
    assert!(!p.carded("u"));
    p.start("b", "Bash", bash("touch x"));
    assert!(p.waits("u"), "a later call does not place it");
    p.wait_out();
    assert!(!p.waits("u"));
    assert_eq!(p.decision("u"), None);
    assert!(p.carded("u"));
    p.no_allow();
    p.shut_down();

    let mut q = Plan::new("plan-unnamed-plan", true, SessionModeChoice::Bypass);
    q.queue(vec![AgentDomainEvent::TurnStarted { turn_id: "t1".into() }]);
    q.pump();
    q.queue(vec![unnamed("u")]);
    q.pump();
    q.queue(enter());
    q.start("b", "Bash", bash("touch x"));
    assert!(q.waits("u"));
    q.wait_out();
    assert!(!q.waits("u"));
    assert_eq!(q.decision("u"), None);
    assert!(q.carded("u"));
    q.no_allow();
    q.shut_down();

    let mut r = Plan::new("plan-unnamed-timeout", true, SessionModeChoice::Bypass);
    r.queue(vec![AgentDomainEvent::TurnStarted { turn_id: "t1".into() }]);
    r.pump();
    r.queue(vec![unnamed("u")]);
    r.pump();
    assert!(r.waits("u"));
    r.wait_out();
    assert!(!r.waits("u"));
    assert_eq!(r.decision("u"), None);
    assert!(r.carded("u"));
    r.no_allow();
    r.shut_down();
}

/// The ingestion folds a request after the window's last drain and the person approves the plan before the next: the
/// approval waits until a drain has looked at the request (`PlanBook::looked_at`), then goes.
#[test]
fn an_approval_waits_for_a_request_no_drain_has_looked_at() {
    let mut p = Plan::new("plan-approval-unseen", false, SessionModeChoice::Manual);
    p.enter();
    p.propose();
    // Folded, not drained: a workflow agent's call, which the sidecar never shows.
    p.queue(vec![request("x", "Bash", bash("touch y"), false)]);
    assert!(p.backend().projection().pending_permissions.contains_key("x"));
    assert!(!p.tab().session.plan.looked_at.contains("x"));
    let t0 = wall_clock_ms();
    let mut send = p
        .set
        .approve_plan(p.tab, ViewId::BASE, "p1", PlanChoice::ApproveManual, SHA, t0)
        .unwrap();
    assert!(
        p.provider.interactions().is_empty(),
        "the approval went before the request was judged"
    );
    assert!(p.set.finish_plan_send(p.tab, &mut send, t0 + 10).is_none());
    assert!(p.provider.interactions().is_empty());
    p.pump();
    assert!(p.tab().session.plan.looked_at.contains("x"));
    assert!(p.waits("x"), "it waits for its call");
    assert!(p.set.finish_plan_send(p.tab, &mut send, t0 + 20).is_none());
    assert!(p.provider.interactions().is_empty());
    p.wait_out();
    assert!(p.carded("x"));
    assert!(p.tab().session.plan.cards.contains("x"));
    assert_eq!(
        p.set.finish_plan_send(p.tab, &mut send, t0 + 30).unwrap().ok(),
        Some(())
    );
    assert_eq!(p.provider.interactions().len(), 1);
    p.shut_down();
}

/// A wall clock that steps back does not hold a request until it has caught up: the wait starts again from the new
/// time, and the window's tick is never told to sleep longer than the wait.
#[test]
fn a_wall_clock_step_back_restarts_the_wait() {
    let mut p = Plan::new("plan-clock-back", true, SessionModeChoice::Bypass);
    p.queue(vec![AgentDomainEvent::TurnStarted { turn_id: "t1".into() }]);
    p.pump();
    p.ask_ahead("y", "Write", write(), false);
    let hour = 3_600_000;
    *p.set
        .get_mut(p.tab)
        .unwrap()
        .session
        .plan
        .unsequenced
        .get_mut("y")
        .unwrap() += hour;
    let release = p.set.next_plan_sequence_release().unwrap();
    assert!(
        release <= Duration::from_millis(crate::agent_backend::PLAN_SEQUENCE_WAIT_MS),
        "{release:?}"
    );
    p.pump();
    let since = p.tab().session.plan.unsequenced["y"];
    assert!(since <= wall_clock_ms(), "the wait starts again from now");
    p.wait_out();
    assert_eq!(p.decision("y"), None);
    assert!(p.carded("y"));
    p.shut_down();
}

/// A chat move's folds rebuild call marks only from the running turn's events, which the window it lands in folds again
/// on top of the records settled before that turn; the marks of calls that began earlier cross beside the records. Here
/// a background subagent's call began while the session planned in the first turn, and the CLI's prompt for it is a
/// card of the second. A card for such a call that `PlanBook::cards` does not list (a resync's card drawn once the
/// session executes again) keeps its call's mark in the new window: entering Bypass there approves nothing of it.
#[test]
fn a_moved_tab_keeps_a_card_whose_call_began_in_plan_mode_from_its_sweeps() {
    let mut p = Plan::new("plan-move-marks", false, SessionModeChoice::Bypass);
    p.provider.open_session("claude-move", &p.dir);
    p.pump();
    p.enter();
    let fetch = serde_json::json!({ "url": "https://example.com" });
    p.queue(vec![
        AgentDomainEvent::ToolCallStarted {
            turn_id: "t1".into(),
            tool_use_id: "toolu_task".into(),
            name: "Task".into(),
            input: serde_json::json!({ "prompt": "look around", "run_in_background": true }),
        },
        AgentDomainEvent::SubagentFrame {
            parent_tool_use_id: "toolu_task".into(),
            frame: Box::new(AgentDomainEvent::ToolCallStarted {
                turn_id: "t1".into(),
                tool_use_id: "toolu_sub".into(),
                name: "WebFetch".into(),
                input: fetch.clone(),
            }),
        },
        AgentDomainEvent::ToolCallCompleted {
            turn_id: "t1".into(),
            tool_use_id: "toolu_task".into(),
            content: serde_json::json!("Running in the background"),
            is_error: false,
        },
        turn_done(),
    ]);
    p.pump();
    assert_eq!(p.phase(), PlanPhase::Planning, "plan mode outlives the turn");
    p.queue(vec![
        AgentDomainEvent::TurnStarted { turn_id: "t2".into() },
        prompt_on("sub2", "toolu_sub", "WebFetch", fetch),
    ]);
    p.pump();
    p.pump();
    assert!(p.carded("sub2"));
    assert_eq!(
        p.backend().projection().call_mark("toolu_sub"),
        Some(agent::CallPlanMark::Planning)
    );
    p.propose();
    p.settle(PlanOutcome::Approved, Some(PlanExecutionMode::Bypass));
    p.cli_back("default");
    p.pump();
    assert_eq!(p.tab().mode(), SessionModeChoice::Bypass);
    assert_eq!(p.phase(), PlanPhase::Executing);
    // The book forgets it, as a resync's card drawn after the session executes again is not in it.
    p.set.get_mut(p.tab).unwrap().session.plan.cards.clear();
    assert!(waiting_cards(p.tab()).is_empty(), "its mark keeps it out here");
    let frozen = super::move_tests::freeze_settled(&mut p.set, p.tab).expect("the tab moves");
    let mut dest = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    let provider = Arc::new(RecordingProvider::planning(false));
    let arrived = super::move_tests::take_up(frozen, &mut dest, provider.clone(), ViewId::BASE);
    for _ in 0..3 {
        dest.pump(&p.dir, true);
    }
    let tab = dest.get(arrived).unwrap();
    assert_eq!(tab.mode(), SessionModeChoice::Bypass);
    let projection = tab.live().unwrap().projection();
    assert!(projection.pending_permissions.contains_key("sub2"));
    assert!(
        projection.call_seen("toolu_sub"),
        "the premise: the call crossed, so the card is placed"
    );
    assert_eq!(
        projection.call_mark("toolu_sub"),
        Some(agent::CallPlanMark::Planning),
        "the mark crossed with the call"
    );
    drop(projection);
    assert!(
        waiting_cards(tab).is_empty(),
        "entering Bypass in the new window would approve a card raised in plan mode"
    );
    assert!(!tab.session.plan.cards.contains("sub2"), "the book crossed as it was");
    assert!(provider.decisions().is_empty());
    for tab in dest.take_all() {
        if let Some(retired) = tab.retired {
            retired.into_backend().shutdown();
        }
    }
    p.shut_down();
}

/// A resync in a Bypass tab drops a `Bash` request whose call has not been folded: the sweep does not allow it. It
/// waits hidden, and once the CLI's `plan` report and the call fold the plan table judges it (left to the CLI's plan
/// mode). The CLI's own prompt for the same call, dropped by a second resync before the call folds, is not swept
/// either, and is a card once the call is seen.
#[test]
fn a_bypass_resync_sweeps_no_request_whose_call_is_unseen() {
    let mut p = Plan::new("plan-resync-unseen", true, SessionModeChoice::Bypass);
    p.queue(vec![AgentDomainEvent::TurnStarted { turn_id: "t1".into() }]);
    p.pump();
    resync_with(&mut p, vec![request("b", "Bash", bash("touch x"), false)]);
    assert_eq!(p.decision("b"), None, "the resync swept a request whose call is unseen");
    assert!(p.waits("b"));
    assert!(!p.carded("b"), "drawn as no card while it waits");
    resync_with(&mut p, vec![prompt_on("b2", "toolu_b", "Bash", bash("touch x"))]);
    assert_eq!(p.decision("b2"), None, "the second resync swept the CLI's prompt");
    assert!(p.waits("b2"));
    p.queue(vec![
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
    p.start("b", "Bash", bash("touch x"));
    assert_eq!(p.decision("b"), Some(PermissionDecision::Defer));
    assert_eq!(p.decision("b2"), None);
    assert!(p.carded("b2"));
    assert!(p.tab().session.plan.cards.contains("b2"));
    assert!(!p.waits("b") && !p.waits("b2"));
    p.no_allow();
    p.shut_down();
}

/// A Manual tab's card for a tool that does not wait for its call is drawn before the call folds; until it has,
/// entering Bypass approves nothing of it (the call may have begun in plan mode), and afterwards it does.
#[test]
fn confirming_bypass_skips_a_card_whose_call_is_unseen() {
    let mut p = Plan::new("plan-confirm-unseen", true, SessionModeChoice::Manual);
    p.queue(vec![AgentDomainEvent::TurnStarted { turn_id: "t1".into() }]);
    p.pump();
    let fetch = serde_json::json!({ "url": "https://example.com" });
    p.ask_ahead("g", "WebFetch", fetch.clone(), false);
    assert!(p.carded("g"));
    assert!(
        waiting_cards(p.tab()).is_empty(),
        "y would approve a card whose call is unseen"
    );
    let ids = vec!["g".to_string()];
    let backend = p.set.get_mut(p.tab).unwrap().live_mut().unwrap();
    assert!(backend.approve_pending(&ids, "in a test").ids.is_empty());
    assert_eq!(p.decision("g"), None);
    p.start("g", "WebFetch", fetch);
    assert_eq!(waiting_cards(p.tab()), ids, "placed: executing when it began");
    p.shut_down();
}

/// A call's mark is dropped once the call completes with no request pending for it, and kept while one is.
#[test]
fn a_finished_calls_mark_is_dropped() {
    let mut p = Plan::new("plan-marks-pruned", true, SessionModeChoice::Manual);
    p.enter();
    let edit = serde_json::json!({ "file_path": "main.rs", "old_string": "a", "new_string": "b" });
    p.start("r", "Read", serde_json::json!({ "file_path": "main.rs" }));
    p.start("e", "Edit", edit.clone());
    p.ask_ahead("e", "Edit", edit, false);
    assert!(p.carded("e"));
    let done = |id: &str| AgentDomainEvent::ToolCallCompleted {
        turn_id: "t1".into(),
        tool_use_id: format!("toolu_{id}"),
        content: serde_json::json!("ok"),
        is_error: false,
    };
    p.queue(vec![done("r"), done("e")]);
    p.pump();
    let projection = p.backend().projection();
    assert_eq!(
        projection.call_mark("toolu_r"),
        None,
        "finished, nothing pending for it"
    );
    assert_eq!(
        projection.call_mark("toolu_e"),
        Some(agent::CallPlanMark::Planning),
        "its card still waits"
    );
    drop(projection);
    p.shut_down();
}

/// A Manual or Auto tab's request for a tool that neither writes nor runs is judged at once: the plan table gives it
/// the tab's own verdict whatever its call turns out to be.
#[test]
fn a_manual_or_auto_read_does_not_wait_for_its_call() {
    let mut p = Plan::new("plan-ahead-read", true, SessionModeChoice::Auto);
    p.queue(vec![AgentDomainEvent::TurnStarted { turn_id: "t1".into() }]);
    p.pump();
    p.ask_ahead("r", "Read", serde_json::json!({ "file_path": "main.rs" }), false);
    assert!(!p.waits("r"));
    assert!(p.decision("r").is_some());
    p.shut_down();
}

/// A card whose call began in plan mode or during a move is a person's alone even where `PlanBook::cards` lost it (a
/// resync after the session executes again): neither entering Bypass nor a sweep approves it.
#[test]
fn the_sweeps_skip_a_card_whose_call_began_outside_executing() {
    let mut p = Plan::new("plan-ahead-sweeps", true, SessionModeChoice::Manual);
    p.queue(vec![AgentDomainEvent::TurnStarted { turn_id: "t1".into() }]);
    p.pump();
    p.queue(vec![AgentDomainEvent::ToolCallStarted {
        turn_id: "t1".into(),
        tool_use_id: "toolu_enter".into(),
        name: "EnterPlanMode".into(),
        input: serde_json::json!({}),
    }]);
    let fetch = serde_json::json!({ "url": "https://example.com" });
    p.start("g", "WebFetch", fetch.clone());
    p.ask("g", "WebFetch", fetch, false);
    assert!(p.carded("g"));
    assert_eq!(
        p.backend().projection().call_mark("toolu_g"),
        Some(agent::CallPlanMark::Moving)
    );
    // The book forgets it, as a resync's card drawn after the session executes again is not in it.
    p.set.get_mut(p.tab).unwrap().session.plan.cards.clear();
    p.queue(vec![turn_done()]);
    p.pump();
    assert!(
        waiting_cards(p.tab()).is_empty(),
        "entering Bypass would approve no card raised while planning"
    );
    let ids = vec!["g".to_string()];
    let backend = p.set.get_mut(p.tab).unwrap().live_mut().unwrap();
    assert!(backend.approve_pending(&ids, "in a test").ids.is_empty());
    assert_eq!(p.decision("g"), None);
    p.shut_down();
}

/// The person approves a plan the dock shows before the CLI's `plan` report has been folded: the approval waits,
/// goes once the report is in, and the CLI's report of its creation mode afterwards ends plan mode with no trip.
#[test]
fn an_approval_before_the_plan_report_waits_and_then_lands() {
    let mut p = Plan::new("plan-fast-approval", false, SessionModeChoice::Manual);
    p.queue(vec![AgentDomainEvent::TurnStarted { turn_id: "t1".into() }]);
    p.pump();
    p.propose();
    assert_eq!(p.phase(), PlanPhase::Executing);
    let t0 = wall_clock_ms();
    let mut send = p
        .set
        .approve_plan(p.tab, ViewId::BASE, "p1", PlanChoice::ApproveManual, SHA, t0)
        .unwrap();
    assert!(p.set.finish_plan_send(p.tab, &mut send, t0 + 100).is_none());
    assert!(
        p.provider.interactions().is_empty(),
        "nothing goes before the CLI says it plans"
    );
    p.queue(vec![
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
    p.pump_until("planning", |p| p.planning.phase == PlanPhase::Planning);
    assert_eq!(
        p.set.finish_plan_send(p.tab, &mut send, t0 + 200).unwrap().ok(),
        Some(())
    );
    assert_eq!(p.provider.interactions().len(), 1);
    p.settle(PlanOutcome::Approved, Some(PlanExecutionMode::Manual));
    p.cli_back("default");
    assert!(p.backend().projection().ungated_cli_mode.is_none(), "no trip");
    assert_eq!(p.tab().mode(), SessionModeChoice::Manual);
    p.shut_down();
}

/// An approval that waits for the CLI's `plan` report is refused, benign, once the guard has passed without it.
#[test]
fn an_approval_waiting_for_the_plan_report_is_refused_at_the_guard() {
    let mut p = Plan::new("plan-fast-guard", false, SessionModeChoice::Manual);
    p.queue(vec![AgentDomainEvent::TurnStarted { turn_id: "t1".into() }]);
    p.pump();
    p.propose();
    let t0 = wall_clock_ms();
    let mut send = p
        .set
        .approve_plan(p.tab, ViewId::BASE, "p1", PlanChoice::ApproveManual, SHA, t0)
        .unwrap();
    let refused = p
        .set
        .finish_plan_send(p.tab, &mut send, t0 + agent::PLAN_TRANSITION_GUARD_MS + 1)
        .unwrap()
        .unwrap_err();
    assert!(refused.benign);
    assert!(
        refused.message.contains("has not reported plan mode"),
        "{}",
        refused.message
    );
    assert!(p.provider.interactions().is_empty());
    assert_eq!(
        p.backend().projection().waiting_plan().unwrap().state,
        PlanState::Waiting
    );
    p.shut_down();
}

/// The CLI's own prompt `id` for the call `call`.
/// A background subagent's `WebFetch` begins while the session plans in the first turn, in a Bypass tab: the
/// events of a subagent call that began in plan mode, and the tab's first turn's end.
fn background_fetch_begun_in_plan_mode(p: &mut Plan, fetch: &serde_json::Value) {
    p.queue(vec![
        AgentDomainEvent::ToolCallStarted {
            turn_id: "t1".into(),
            tool_use_id: "toolu_task".into(),
            name: "Task".into(),
            input: serde_json::json!({ "prompt": "look around", "run_in_background": true }),
        },
        AgentDomainEvent::SubagentFrame {
            parent_tool_use_id: "toolu_task".into(),
            frame: Box::new(AgentDomainEvent::ToolCallStarted {
                turn_id: "t1".into(),
                tool_use_id: "toolu_sub".into(),
                name: "WebFetch".into(),
                input: fetch.clone(),
            }),
        },
        AgentDomainEvent::ToolCallCompleted {
            turn_id: "t1".into(),
            tool_use_id: "toolu_task".into(),
            content: serde_json::json!("Running in the background"),
            is_error: false,
        },
    ]);
    p.pump();
    assert_eq!(
        p.backend().projection().call_mark("toolu_sub"),
        Some(agent::CallPlanMark::Planning)
    );
}

fn shut_down_set(set: &mut TabSet) {
    for tab in set.take_all() {
        if let Some(retired) = tab.retired {
            retired.into_backend().shutdown();
        }
    }
}

/// A call that began while the session planned keeps its mark across a chat move although it began before the
/// running turn, whose events alone the new window folds again. Here a background subagent's call begins in the first
/// turn and its hook request is answered on a card there; the plan is approved into Bypass in the second turn, and the
/// tab moves with nothing pending, before the CLI's own prompt for that same call. In the new window that prompt is a
/// card, as it would have been in the window the tab left; Bypass allows nothing of it.
#[test]
fn a_prompt_after_a_move_for_a_call_begun_in_plan_mode_in_an_earlier_turn_is_a_card() {
    let mut p = Plan::new("plan-move-later-prompt", false, SessionModeChoice::Bypass);
    p.provider.open_session("claude-move", &p.dir);
    p.pump();
    p.enter();
    let fetch = serde_json::json!({ "url": "https://example.com" });
    background_fetch_begun_in_plan_mode(&mut p, &fetch);
    p.queue(vec![AgentDomainEvent::PermissionRequested {
        permission_id: "hook".into(),
        tool_use_id: Some("toolu_sub".into()),
        tool_name: "WebFetch".into(),
        input: fetch.clone(),
        provider_prompt: None,
    }]);
    p.pump();
    p.pump();
    assert!(p.carded("hook"), "Bypass answers nothing itself in plan mode");
    p.set
        .answer_card(p.tab, ViewId::BASE, "hook", PermissionDecision::Allow)
        .unwrap();
    assert_eq!(p.decision("hook"), Some(PermissionDecision::Allow));
    p.queue(vec![
        AgentDomainEvent::PermissionResolved {
            permission_id: "hook".into(),
            outcome: agent::PermissionOutcome::Allowed,
        },
        turn_done(),
    ]);
    p.pump();
    p.queue(vec![AgentDomainEvent::TurnStarted { turn_id: "t2".into() }]);
    p.pump();
    p.propose();
    p.settle(PlanOutcome::Approved, Some(PlanExecutionMode::Bypass));
    p.cli_back("default");
    p.pump();
    assert_eq!(p.tab().mode(), SessionModeChoice::Bypass);
    assert_eq!(p.phase(), PlanPhase::Executing);
    assert!(
        p.backend().projection().pending_permissions.is_empty(),
        "the premise: nothing is pending at the move"
    );
    assert!(
        p.backend().projection().active_turn_id.is_some(),
        "the second turn runs"
    );
    let frozen = super::move_tests::freeze_settled(&mut p.set, p.tab).expect("the tab moves");
    let mut dest = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    let provider = Arc::new(RecordingProvider::planning(false));
    let arrived = super::move_tests::take_up(frozen, &mut dest, provider.clone(), ViewId::BASE);
    dest.pump(&p.dir, true);
    assert!(
        dest.get(arrived)
            .unwrap()
            .live()
            .unwrap()
            .projection()
            .call_seen("toolu_sub"),
        "the premise: the call crossed in the seed"
    );
    provider.queue_all(vec![prompt_on("sub2", "toolu_sub", "WebFetch", fetch)]);
    provider.wait_until_folded();
    for _ in 0..3 {
        dest.pump(&p.dir, true);
    }
    let tab = dest.get(arrived).unwrap();
    assert_eq!(tab.mode(), SessionModeChoice::Bypass);
    assert!(
        provider.decisions().iter().all(|(id, _)| id != "sub2"),
        "Bypass in the new window answered the CLI's prompt for a call made in plan mode: {:?}",
        provider.decisions()
    );
    assert!(tab.attention.card_ids().contains(&"sub2".to_string()), "it is a card");
    assert!(waiting_cards(tab).is_empty());
    let projection = tab.live().unwrap().projection();
    assert!(projection.pending_permissions.contains_key("sub2"));
    assert_eq!(
        projection.call_mark("toolu_sub"),
        Some(agent::CallPlanMark::Planning),
        "the call's mark crossed with it"
    );
    drop(projection);
    shut_down_set(&mut dest);
    p.shut_down();
}

/// A request is never both a plan card and waiting for its call. Here a background subagent's call began while the
/// session planned; `/clear` dropped its record and kept its mark, so the CLI's prompt for it waits for a call that
/// will not come again, and the tab moves within the wait. In the new window the request is drawn as a card once the
/// wait is over, and an approval no longer waits for it.
#[test]
fn a_request_waiting_for_its_call_across_a_move_is_drawn_after_the_wait() {
    let mut p = Plan::new("plan-move-unsequenced", false, SessionModeChoice::Bypass);
    p.provider.open_session("claude-move", &p.dir);
    p.pump();
    p.enter();
    let fetch = serde_json::json!({ "url": "https://example.com" });
    background_fetch_begun_in_plan_mode(&mut p, &fetch);
    p.queue(vec![turn_done()]);
    p.pump();
    p.queue(vec![
        AgentDomainEvent::TurnStarted { turn_id: "t2".into() },
        AgentDomainEvent::ConversationReset {
            new_conversation_id: String::new(),
            trigger: Some("clear".into()),
        },
        prompt_on("sub2", "toolu_sub", "WebFetch", fetch),
    ]);
    p.pump();
    p.pump();
    {
        let projection = p.backend().projection();
        assert!(
            !projection.call_seen("toolu_sub"),
            "the premise: /clear dropped the call's record"
        );
        assert!(
            projection.began_outside_executing(Some("toolu_sub")),
            "and kept its mark"
        );
    }
    assert!(p.waits("sub2"), "the premise: the prompt waits for its call");
    assert_eq!(p.phase(), PlanPhase::Planning);
    let frozen = super::move_tests::freeze_settled(&mut p.set, p.tab).expect("the tab moves");
    let mut dest = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    let provider = Arc::new(RecordingProvider::planning(false));
    let arrived = super::move_tests::take_up(frozen, &mut dest, provider.clone(), ViewId::BASE);
    dest.pump(&p.dir, true);
    for since in dest.get_mut(arrived).unwrap().session.plan.unsequenced.values_mut() {
        *since = since.saturating_sub(crate::agent_backend::PLAN_SEQUENCE_WAIT_MS + 1);
    }
    for _ in 0..3 {
        dest.pump(&p.dir, true);
    }
    let tab = dest.get(arrived).unwrap();
    assert!(
        !tab.session.plan.unsequenced.contains_key("sub2"),
        "still waiting for its call, it holds back every approval of a plan"
    );
    assert!(tab.session.plan.looked_at.contains("sub2"));
    let hidden = tab.session.hidden_permissions(&tab.live().unwrap().projection());
    assert!(!hidden.contains("sub2"), "the card stays hidden");
    assert!(tab.attention.card_ids().contains(&"sub2".to_string()), "it is a card");
    assert!(tab.session.plan.cards.contains("sub2"));
    assert!(provider.decisions().is_empty());
    shut_down_set(&mut dest);
    p.shut_down();
}

/// A plan card that is also among the requests waiting for their call is drawn at once and leaves them: a drain that
/// took it as a card and left it waiting would keep it hidden, and every approval of the plan held back, for good.
#[test]
fn a_plan_card_never_stays_among_the_requests_waiting_for_their_call() {
    let mut p = Plan::new("plan-card-unsequenced", false, SessionModeChoice::Bypass);
    p.enter();
    p.ask_ahead("x", "Write", write(), false);
    assert!(p.waits("x"));
    assert!(!p.carded("x"));
    p.set.get_mut(p.tab).unwrap().session.plan.cards.insert("x".into());
    p.pump();
    p.pump();
    p.wait_out();
    assert!(!p.waits("x"), "a card waits for nothing");
    let hidden = p.tab().session.hidden_permissions(&p.backend().projection());
    assert!(!hidden.contains("x"));
    assert!(p.carded("x"));
    assert_eq!(p.decision("x"), None);
    p.shut_down();
}

fn prompt_on(id: &str, call: &str, tool: &str, input: serde_json::Value) -> AgentDomainEvent {
    match request(id, tool, input, true) {
        AgentDomainEvent::PermissionRequested {
            permission_id,
            tool_name,
            input,
            provider_prompt,
            ..
        } => AgentDomainEvent::PermissionRequested {
            permission_id,
            tool_use_id: Some(call.into()),
            tool_name,
            input,
            provider_prompt,
        },
        _ => unreachable!("a request"),
    }
}

fn turn_done() -> AgentDomainEvent {
    AgentDomainEvent::TurnCompleted {
        turn_id: "t1".into(),
        outcome: agent::TurnOutcome::Completed,
        result_text: String::new(),
        stop_reason: None,
        usage: None,
        detail: Default::default(),
    }
}

/// The person's own leave of a Bypass-origin round lands the tab in Manual. When the CLI's report of its way out, and
/// a `Write` asked after it, fold after the pump last looked at plan mode and a resync then draws what is pending, the
/// tab still shows Bypass: the resync's bypass sweep must not answer the `Write` under it.
#[test]
fn a_resync_right_after_a_leave_sweeps_nothing_under_the_mode_plan_mode_began_in() {
    let mut p = Plan::new("plan-leave-resync", true, SessionModeChoice::Bypass);
    p.enter();
    p.queue(vec![turn_done()]);
    p.pump();
    let switch = p
        .set
        .set_planning(p.tab, ViewId::BASE, false, wall_clock_ms())
        .unwrap()
        .unwrap();
    assert_eq!(p.set.finish_planning_switch(p.tab, &switch).unwrap().ok(), Some(()));
    assert!(matches!(p.phase(), PlanPhase::Leaving { .. }));
    let provider = p.provider.clone();
    p.set.before_resync_sweep = Some(Box::new(move |_| {
        provider.queue_all(vec![
            AgentDomainEvent::CliPermissionMode {
                reported: "auto".into(),
            },
            AgentDomainEvent::TurnStarted { turn_id: "t2".into() },
            AgentDomainEvent::ToolCallStarted {
                turn_id: "t2".into(),
                tool_use_id: "toolu_w".into(),
                name: "Write".into(),
                input: write(),
            },
            request("w", "Write", write(), false),
        ]);
        provider.wait_until_folded();
    }));
    resync_with(&mut p, Vec::new());
    assert_eq!(p.phase(), PlanPhase::Executing);
    assert_eq!(p.decision("w"), None, "the resync swept the Write under Bypass");
    p.pump();
    p.pump();
    assert_eq!(
        p.tab().mode(),
        SessionModeChoice::Manual,
        "the person's own leave lands in Manual"
    );
    assert_eq!(p.decision("w"), None);
    p.shut_down();
}

/// A restored tab asked to start in plan mode whose first message is queued (`Ctrl+x Enter`) rather than sent:
/// the switch goes first and the message waits behind it, then goes once the CLI is in plan mode.
#[test]
fn a_queued_first_message_waits_for_a_restored_tabs_plan_mode() {
    let mut p = Plan::new("plan-first-queued", true, SessionModeChoice::Auto);
    p.set.get_mut(p.tab).unwrap().plan_on_start = true;
    p.set
        .queue_message(
            p.tab,
            "plan the refactor",
            "plan the refactor".into(),
            wall_clock_ms(),
            true,
            agent::PromptOrigin::Typed,
        )
        .unwrap();
    assert!(p.set.flush_queue(p.tab).is_none(), "held behind the switch");
    assert_eq!(p.provider.planning_calls(), vec![true]);
    assert!(p.provider.turns().is_empty());
    p.pump();
    assert!(p.set.release_after_card().is_empty());
    assert!(p.provider.turns().is_empty(), "held until the CLI is in plan mode");
    p.queue(vec![AgentDomainEvent::CliPlanMode {
        source: "CliModeReported".into(),
    }]);
    p.pump_until("planning", |pr| pr.planning.phase == PlanPhase::Planning);
    assert_eq!(p.set.release_after_card().len(), 1);
    assert_eq!(p.provider.turns().len(), 1);
    assert_eq!(p.provider.planning_calls(), vec![true], "one switch");
    p.shut_down();
}

/// A plan proposed in a turn the CLI started itself can be folded before that turn's start: the sidecar's
/// interactions overtake its message events. With no turn open, a flush of the queue (a companion's message,
/// `Ctrl+Enter` while idle) waits behind the plan as it waits behind a card or a question, and goes once the plan
/// is settled.
#[test]
fn a_queue_flush_waits_behind_a_plan_whose_turn_is_not_seen_yet() {
    let mut p = Plan::new("plan-queue-no-turn", true, SessionModeChoice::Manual);
    // Plan mode outlives the turn that entered it.
    p.enter();
    p.queue(vec![AgentDomainEvent::TurnCompleted {
        turn_id: "t1".into(),
        outcome: agent::TurnOutcome::Completed,
        result_text: String::new(),
        stop_reason: None,
        usage: None,
        detail: Default::default(),
    }]);
    p.pump_until("the turn's end", |pr| pr.active_turn_id.is_none());
    assert_eq!(p.phase(), PlanPhase::Planning);
    p.propose();
    assert!(p.backend().projection().active_turn_id.is_none(), "no turn is open");
    p.set
        .queue_message(
            p.tab,
            "companion",
            "companion".into(),
            wall_clock_ms(),
            false,
            agent::PromptOrigin::Composed,
        )
        .unwrap();
    assert!(p.set.flush_queue(p.tab).is_none(), "held behind the plan");
    assert!(p.provider.turns().is_empty());
    p.settle(PlanOutcome::Interrupted, None);
    assert!(p.backend().projection().waiting_plan().is_none());
    assert!(p.set.flush_queue(p.tab).is_some(), "goes once the plan is settled");
    assert_eq!(p.provider.turns(), vec!["companion"]);
    p.shut_down();
}

/// The approval a tab took up last is a `seq` of its session's projection: a new session behind the tab (a reset, a
/// reconnect) numbers its own, so an approval there is taken up however low its `seq`.
#[test]
fn a_new_session_takes_up_its_own_approvals_whatever_the_old_one_numbered() {
    let mut p = Plan::new("plan-applied-seq", true, SessionModeChoice::Manual);
    p.set.get_mut(p.tab).unwrap().plan_mode_applied_seq = Some(1_000_000);
    let provider = Arc::new(RecordingProvider::planning(true));
    let conversation = AgentConversation::create(
        provider.clone(),
        &p.dir,
        agent::setting_sources::ProjectTrust::Untrusted,
    )
    .unwrap();
    let old = p.provider.clone();
    p.set.get_mut(p.tab).unwrap().install_live(AgentBackend::Conversation {
        agent: AgentId::Claude,
        conversation: Box::new(conversation),
    });
    drop(old);
    p.provider = provider;
    assert_eq!(p.tab().plan_mode_applied_seq, None);
    p.queue(vec![AgentDomainEvent::CliPermissionMode {
        reported: "auto".into(),
    }]);
    p.pump_until("the CLI's mode", |pr| pr.cli_mode_reported.is_some());
    p.enter();
    p.propose();
    p.settle(PlanOutcome::Approved, Some(PlanExecutionMode::Auto));
    p.cli_back("auto");
    assert_eq!(
        p.tab().mode(),
        SessionModeChoice::Auto,
        "the approval into Auto was taken up"
    );
    p.shut_down();
}
