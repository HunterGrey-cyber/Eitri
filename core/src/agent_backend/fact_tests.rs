//! `decide` on a remote tab with the server's facts (`GateRoot::RemoteWithFacts`), beside a local tab at the same
//! root: the facts are computed with the disk half on a real directory, as the helper computes them on the server.

use super::*;
use std::os::unix::fs::PermissionsExt;

fn workspace(label: &str) -> PathBuf {
    agent::state_dirs::redirect_state_to_a_test_root();
    let dir = agent::state_dirs::test_workspace_dir(label);
    std::fs::write(dir.join("main.rs"), "fn main() {}\n").unwrap();
    dir
}

/// A root others can write to: no boundary, here and on a server.
fn open_workspace(label: &str) -> PathBuf {
    let dir = workspace(label);
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(!root_is_a_boundary(&dir));
    dir
}

fn fact_at(root: &Path, tool: &str, input: &serde_json::Value) -> agent::GateFact {
    let around = agent::FactSurroundings {
        home: std::env::var_os("HOME").map(PathBuf::from),
        identity: agent::project_boundary::Identity::current(),
        git_location_from_environment: agent::permission_policy::GIT_LOCATION_VARIABLES
            .iter()
            .any(|name| std::env::var_os(name).is_some()),
    };
    agent::gate_fact(tool, input, root, &around, &agent::EitriPlaces::default())
}

/// One judgement: the decision, and whether it read the fact or missed it.
struct Judged {
    decision: Option<PermissionDecision>,
    read: bool,
    missing: bool,
}

/// How a request is judged besides the root.
#[derive(Clone, Copy)]
struct Case<'a> {
    mode: PermissionMode,
    planning: Option<PlanGate>,
    /// Auto on a session running the CLI's auto mode.
    may_defer: bool,
    tool: &'a str,
}

fn judge(root: GateRoot<'_>, fact: Option<&agent::GateFact>, case: Case<'_>, input: &serde_json::Value) -> Judged {
    let rules = agent::PrefixRules::default().with(agent::PrefixRule::parse("Bash(make *)").unwrap());
    let facts = FactRead::new(fact);
    let decision = decide(
        GateRequest {
            permission_id: "p1",
            tool_use_id: &Some("tu-1".into()),
            tool_name: case.tool,
            input,
            provider_prompt: None,
        },
        Judge {
            mode: case.mode,
            external: false,
            defers_to_the_cli: case.may_defer && gate_root_is_a_boundary(root),
            may_defer: case.may_defer,
            own_tools: None,
            rules: &rules,
            project_root: root,
            approvals: &HumanApprovals::default(),
            planning: case.planning,
            places: &agent::EitriPlaces::default(),
            facts: &facts,
        },
    )
    .map(|planned| planned.decision);
    Judged {
        decision,
        read: facts.read(),
        missing: facts.missing(),
    }
}

fn calls(root: &Path) -> Vec<(&'static str, serde_json::Value)> {
    vec![
        ("Read", serde_json::json!({"file_path": root.join("main.rs")})),
        ("Read", serde_json::json!({"file_path": "/etc/passwd"})),
        ("Bash", serde_json::json!({"command": "ls"})),
        ("Bash", serde_json::json!({"command": "make build"})),
        ("Bash", serde_json::json!({"command": "rm -rf target"})),
        (
            "Write",
            serde_json::json!({"file_path": root.join("new.rs"), "content": "x"}),
        ),
        (
            "Edit",
            serde_json::json!({"file_path": root.join("main.rs"), "old_string": "a", "new_string": "b"}),
        ),
        (
            "NotebookEdit",
            serde_json::json!({"notebook_path": root.join("n.ipynb"), "new_source": "x"}),
        ),
        ("TodoWrite", serde_json::json!({"todos": []})),
        ("WebFetch", serde_json::json!({"url": "https://example.org"})),
        ("mcp__other__tool", serde_json::json!({"path": root.join("main.rs")})),
    ]
}

/// Every mode, with and without the CLI's auto mode, at a boundary and at a root that is none: the decision the
/// server's fact gives is the local tab's.
#[test]
fn remote_facts_decide_as_a_local_root_does_in_every_mode() {
    for root in [workspace("facts-decide"), open_workspace("facts-decide-open")] {
        for (tool, input) in calls(&root) {
            let fact = fact_at(&root, tool, &input);
            for mode in [PermissionMode::Manual, PermissionMode::Auto, PermissionMode::Bypass] {
                for may_defer in [false, mode == PermissionMode::Auto] {
                    let case = Case {
                        mode,
                        planning: None,
                        may_defer,
                        tool,
                    };
                    let local = judge(GateRoot::Local(&root), None, case, &input);
                    let remote = judge(GateRoot::RemoteWithFacts, Some(&fact), case, &input);
                    assert_eq!(local.decision, remote.decision, "{mode:?} {may_defer} {tool} {input}");
                    assert!(!remote.missing);
                }
            }
        }
    }
}

/// Without its fact a call judged by its paths is no answer and says the fact was missing; a call decided by its
/// name, and every call in Bypass, read none.
#[test]
fn a_missing_fact_is_noted_and_nothing_decided_by_name_or_bypass_needs_one() {
    let root = workspace("facts-missing");
    let case = |mode, tool| Case {
        mode,
        planning: None,
        may_defer: false,
        tool,
    };
    let inside = serde_json::json!({"file_path": root.join("main.rs")});
    let read = judge(
        GateRoot::RemoteWithFacts,
        None,
        case(PermissionMode::Auto, "Read"),
        &inside,
    );
    assert!(read.decision.is_none() && read.missing && !read.read);
    let todo = serde_json::json!({"todos": []});
    let named = judge(
        GateRoot::RemoteWithFacts,
        None,
        case(PermissionMode::Manual, "TodoWrite"),
        &todo,
    );
    assert_eq!(named.decision, Some(PermissionDecision::Allow));
    assert!(!named.missing && !named.read);
    let fetch = serde_json::json!({"url": "https://example.org"});
    let carded = judge(
        GateRoot::RemoteWithFacts,
        None,
        case(PermissionMode::Auto, "WebFetch"),
        &fetch,
    );
    assert!(
        carded.decision.is_none() && !carded.missing,
        "a card by its name, not a wait"
    );
    let rm = serde_json::json!({"command": "rm -rf target"});
    let bypass = judge(
        GateRoot::RemoteWithFacts,
        None,
        case(PermissionMode::Bypass, "Bash"),
        &rm,
    );
    assert_eq!(bypass.decision, Some(PermissionDecision::Allow));
    assert!(!bypass.missing && !bypass.read);
    // A call carded by its name that a boundary could leave to the CLI reads the fact, as the places check comes
    // first there.
    let deferring = Case {
        may_defer: true,
        ..case(PermissionMode::Auto, "WebFetch")
    };
    assert!(judge(GateRoot::RemoteWithFacts, None, deferring, &fetch).missing);
    // An answer that came from the fact says so: the gate checks it fresh before it goes.
    let fact = fact_at(&root, "Read", &inside);
    let allowed = judge(
        GateRoot::RemoteWithFacts,
        Some(&fact),
        case(PermissionMode::Auto, "Read"),
        &inside,
    );
    assert_eq!(allowed.decision, Some(PermissionDecision::Allow));
    assert!(allowed.read);
}

/// Plan mode's table reads the server's boundary in place of the laptop's and nothing else: every origin, main
/// thread or not, CLI auto or not, at a boundary and at a root that is none, decides as a local tab there.
#[test]
fn plan_mode_reads_the_servers_boundary_and_nothing_else_changes() {
    for root in [workspace("facts-plan"), open_workspace("facts-plan-open")] {
        for (tool, input) in calls(&root) {
            let fact = fact_at(&root, tool, &input);
            for origin in [PermissionMode::Manual, PermissionMode::Auto, PermissionMode::Bypass] {
                for main_thread in [false, true] {
                    for cli_runs_auto in [false, true] {
                        let case = Case {
                            mode: origin,
                            planning: Some(PlanGate {
                                origin,
                                cli_runs_auto,
                                main_thread,
                            }),
                            may_defer: false,
                            tool,
                        };
                        let local = judge(GateRoot::Local(&root), None, case, &input);
                        let remote = judge(GateRoot::RemoteWithFacts, Some(&fact), case, &input);
                        assert_eq!(
                            local.decision, remote.decision,
                            "{origin:?} main={main_thread} auto={cli_runs_auto} {tool}"
                        );
                        // A deferral to the CLI's plan mode came from the server's boundary.
                        if remote.decision == Some(PermissionDecision::Defer) {
                            assert!(remote.read);
                        }
                    }
                }
            }
        }
    }
    // A main-thread `Write` is left to the CLI's plan mode only at a root the server calls a boundary.
    let root = workspace("facts-plan-write");
    let input = serde_json::json!({"file_path": root.join("plan.md"), "content": "x"});
    let mut fact = fact_at(&root, "Write", &input);
    let case = Case {
        mode: PermissionMode::Manual,
        planning: Some(PlanGate {
            origin: PermissionMode::Manual,
            cli_runs_auto: false,
            main_thread: true,
        }),
        may_defer: false,
        tool: "Write",
    };
    assert_eq!(
        judge(GateRoot::RemoteWithFacts, Some(&fact), case, &input).decision,
        Some(PermissionDecision::Defer)
    );
    fact.root_is_boundary = false;
    assert_eq!(
        judge(GateRoot::RemoteWithFacts, Some(&fact), case, &input).decision,
        None
    );
    assert!(judge(GateRoot::RemoteWithFacts, None, case, &input).missing);
}

/// A request whose fresh check is over but which the drain draws before judging it (here: the session reported a
/// policy it was not asked for) takes its wait with it: none is left over, hidden, with its time up.
#[test]
fn a_request_drawn_without_being_judged_leaves_no_fact_wait() {
    let dir = workspace("facts-drawn-unjudged");
    let provider = std::sync::Arc::new(crate::test_providers::RecordingProvider::default());
    provider.set_link(Some(agent::RemoteLinkState::Live));
    provider.serve_facts(&dir);
    let input = serde_json::json!({"file_path": dir.join("main.rs")});
    let fact = fact_at(&dir, "Read", &input);
    provider.set_fact(
        "p1",
        agent::gate_facts::GateFactState::Have(std::sync::Arc::new(agent::gate_facts::ReceivedFact {
            fact: fact.clone(),
            request_digest: [1; 32],
            received: std::time::Instant::now(),
        })),
    );
    let conversation = AgentConversation::create(provider.clone(), &dir, ProjectTrust::Untrusted).unwrap();
    let mut backend = AgentBackend::Conversation {
        agent: AgentId::Claude,
        conversation: Box::new(conversation),
    };
    let mut approvals = HumanApprovals::default();
    let rules = agent::PrefixRules::default();
    let drain = |backend: &mut AgentBackend, approvals: &mut HumanApprovals| {
        provider.wait_until_folded();
        backend
            .take_revised_ui_delivery_after_answers(
                GateRoot::RemoteWithFacts,
                &rules,
                PermissionMode::Auto,
                &mut BTreeSet::new(),
                approvals,
                &BTreeSet::new(),
                &mut AnsweredForYou::default(),
                &mut PlanBook::default(),
            )
            .without_revisions()
    };
    provider.queue(AgentDomainEvent::PermissionRequested {
        permission_id: "p1".into(),
        tool_use_id: Some("tu-1".into()),
        tool_name: "Read".into(),
        input: input.clone(),
        provider_prompt: None,
    });
    for _ in 0..20 {
        drain(&mut backend, &mut approvals);
        if !provider.fresh_checks().is_empty() {
            break;
        }
    }
    assert_eq!(provider.fresh_checks(), ["p1"]);
    assert!(approvals.next_fact_release().is_some());
    provider.queue(AgentDomainEvent::UngatedCliMode {
        reported: "bypassPermissions".into(),
        detail: "SessionReady".into(),
    });
    drain(&mut backend, &mut approvals);
    provider.answer_fresh_checks(agent::gate_facts::FreshFact::Both {
        first: fact.clone(),
        fresh: fact,
    });
    // Drawn by a snapshot, as every request kept from the page and judged a card later is.
    let delivered = drain(&mut backend, &mut approvals);
    assert!(matches!(delivered, agent::UiDelivery::Resync), "drawn: {delivered:?}");
    assert_eq!(approvals.next_fact_release(), None, "no wait left behind");
    assert!(provider.decisions().is_empty(), "nothing answered");
    backend.shutdown();
}
