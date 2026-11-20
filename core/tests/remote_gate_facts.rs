//! A remote tab's gate with the server's path facts, driven through a provider that stands for a helper serving
//! them (`RecordingProvider::serve_facts`): the facts are computed here with the disk half on a real directory, as
//! the helper computes them on the server. No ssh, no display, no real `claude`.
#![cfg(target_os = "linux")]

use agent::gate_facts::{FreshFact, GateFactState, ReceivedFact, RootFact};
use agent::project_boundary::{Identity, NotABoundary};
use agent::setting_sources::ProjectTrust;
use agent::{AgentConversation, AgentDomainEvent, FactSurroundings, GateFact, PermissionDecision, RemoteLinkState};
use eitri_core::agent_backend::{AgentBackend, AgentId, BackendKind};
use eitri_core::agent_bridge::SessionModeChoice;
use eitri_core::remote_project::TabOrigin;
use eitri_core::tab_set::{ModeCycle, TabBackend, TabSet};
use eitri_core::tabs::{TabId, ViewId};
use eitri_core::test_providers::RecordingProvider;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const HOST: &str = "example-host";
const UNAVAILABLE: &str = "this tab runs on example-host; path checks on the server are not available yet";

fn workspace(label: &str) -> PathBuf {
    agent::state_dirs::redirect_state_to_a_test_root();
    let dir = agent::state_dirs::test_workspace_dir(label);
    std::fs::write(dir.join("main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(dir.join("notes.txt"), "notes\n").unwrap();
    dir
}

fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The disk half on this machine's disk, with this process's surroundings: what a helper running here would send.
fn fact_at(root: &Path, tool: &str, input: &Value) -> GateFact {
    let around = FactSurroundings {
        home: std::env::var_os("HOME").map(PathBuf::from),
        identity: Identity::current(),
        git_location_from_environment: agent::permission_policy::GIT_LOCATION_VARIABLES
            .iter()
            .any(|name| std::env::var_os(name).is_some()),
    };
    agent::gate_fact(tool, input, root, &around, &agent::EitriPlaces::from_process())
}

fn have(fact: GateFact) -> GateFactState {
    have_named(fact, [1; 32])
}

/// A fact for the request whose digest is `digest`.
fn have_named(fact: GateFact, digest: [u8; 32]) -> GateFactState {
    GateFactState::Have(Arc::new(ReceivedFact {
        fact,
        request_digest: digest,
        received: Instant::now(),
    }))
}

fn request(id: &str, tool: &str, input: Value) -> AgentDomainEvent {
    AgentDomainEvent::PermissionRequested {
        permission_id: id.into(),
        tool_use_id: Some(format!("tu-{id}")),
        tool_name: tool.into(),
        input,
        provider_prompt: None,
    }
}

/// One tab and the provider under it.
struct Bench {
    dir: PathBuf,
    set: TabSet,
    tab: TabId,
    provider: Arc<RecordingProvider>,
}

impl Bench {
    /// A tab on `dir` in `mode`: remote when `remote` (its link live, the trust scan's root named as `dir`), serving
    /// the server's facts about `dir` when `facts`.
    fn new(label: &str, mode: SessionModeChoice, provider: RecordingProvider, remote: bool, facts: bool) -> Bench {
        let dir = workspace(label);
        let mut set = TabSet::new(BackendKind::Sidecar, mode);
        if remote {
            set.set_project_origin(TabOrigin::Remote {
                host: HOST.into(),
                host_key_sha256: "SHA256:example".into(),
                root: dir.clone(),
            })
            .unwrap();
        }
        let tab = set.active();
        let provider = Arc::new(provider);
        if remote {
            provider.set_link(Some(RemoteLinkState::Live));
        }
        if facts {
            provider.serve_facts(&dir);
        }
        let conversation =
            AgentConversation::create_with_tools(provider.clone(), &dir, ProjectTrust::Untrusted, None).unwrap();
        set.get_mut(tab)
            .unwrap()
            .set_backend(TabBackend::Live(AgentBackend::Conversation {
                agent: AgentId::Claude,
                conversation: Box::new(conversation),
            }));
        if remote {
            set.set_server_root(tab, Some(dir.clone()));
        }
        Bench {
            dir,
            set,
            tab,
            provider,
        }
    }

    fn remote(label: &str, mode: SessionModeChoice) -> Bench {
        Bench::new(label, mode, RecordingProvider::default(), true, true)
    }

    fn pump(&mut self) {
        let dir = self.dir.clone();
        self.set.pump(&dir, true);
    }

    fn pending(&self, id: &str) -> bool {
        self.set
            .get(self.tab)
            .unwrap()
            .live()
            .unwrap()
            .projection()
            .pending_permissions
            .contains_key(id)
    }

    /// Queues `event` and pumps until its request is folded and has been looked at once.
    fn ask(&mut self, event: AgentDomainEvent) {
        let AgentDomainEvent::PermissionRequested { permission_id, .. } = &event else {
            panic!("a request");
        };
        let id = permission_id.clone();
        self.provider.queue(event);
        until("the request folded", || {
            self.pump();
            self.pending(&id)
        });
        self.pump();
    }

    fn answer(&self, id: &str) -> Option<PermissionDecision> {
        self.provider
            .decisions()
            .into_iter()
            .find(|(answered, _)| answered == id)
            .map(|(_, decision)| decision)
    }

    fn answers(&self, id: &str) -> usize {
        self.provider
            .decisions()
            .iter()
            .filter(|(answered, _)| answered == id)
            .count()
    }

    fn card(&self, id: &str) -> bool {
        self.set.get(self.tab).unwrap().card_is_waiting(id)
    }

    /// Pumps until `id` is answered or drawn as a card: its answer, or `None` for a card.
    fn settle(&mut self, id: &str) -> Option<PermissionDecision> {
        until("an answer or a card", || {
            self.pump();
            self.answer(id).is_some() || self.card(id)
        });
        self.answer(id)
    }

    fn mode(&mut self, to: SessionModeChoice) {
        while self.set.get(self.tab).unwrap().mode() != to {
            match self.set.cycle_mode(self.tab, ViewId::BASE).unwrap() {
                ModeCycle::Changed(_) => {}
                ModeCycle::Confirm(plan) => {
                    self.set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE).unwrap();
                }
            }
        }
    }

    fn note(&self) -> Option<String> {
        self.set.get(self.tab).unwrap().root_note()
    }

    fn shut_down(mut self) {
        if let Some(backend) = self.set.get_mut(self.tab).and_then(|tab| tab.live_mut()) {
            backend.shutdown();
        }
    }
}

/// The calls the comparison makes: in and out of the root, read-only and changing commands, the edit tools, tools
/// allowed and carded by name, and a write into Eitri's own configuration.
fn scripted_calls(root: &Path) -> Vec<(&'static str, Value)> {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    vec![
        ("Read", json!({"file_path": root.join("main.rs")})),
        ("Read", json!({"file_path": "/etc/passwd"})),
        ("Glob", json!({"pattern": "**/*.rs"})),
        ("Grep", json!({"pattern": "main", "path": root})),
        ("Bash", json!({"command": "ls"})),
        ("Bash", json!({"command": "make build"})),
        ("Bash", json!({"command": "cat /etc/passwd"})),
        ("Bash", json!({"command": "rm -rf target"})),
        ("Write", json!({"file_path": root.join("new.rs"), "content": "x"})),
        (
            "Edit",
            json!({"file_path": root.join("main.rs"), "old_string": "main", "new_string": "start"}),
        ),
        ("Write", json!({"file_path": "/tmp/outside.txt", "content": "x"})),
        (
            "Write",
            json!({"file_path": home.join(".config/eitri/init.lua"), "content": "x"}),
        ),
        ("TodoWrite", json!({"todos": []})),
        ("WebFetch", json!({"url": "https://example.org"})),
        ("mcp__other__tool", json!({"path": root.join("main.rs")})),
    ]
}

fn rules() -> agent::PrefixRules {
    agent::PrefixRules::default().with(agent::PrefixRule::parse("Bash(make *)").unwrap())
}

/// What a tab answers to the scripted calls: each call's answer, or `None` for a card.
fn verdicts(bench: &mut Bench, root_for_facts: Option<&Path>) -> Vec<Option<PermissionDecision>> {
    bench.set.set_rules(rules());
    let calls = scripted_calls(root_for_facts.unwrap_or(&bench.dir.clone()));
    let mut out = Vec::new();
    for (n, (tool, input)) in calls.into_iter().enumerate() {
        let id = format!("p{n}");
        if let Some(root) = root_for_facts {
            bench.provider.set_fact(&id, have(fact_at(root, tool, &input)));
        }
        bench.ask(request(&id, tool, input));
        out.push(bench.settle(&id));
    }
    out
}

#[test]
fn remote_tab_cards_what_a_local_tab_cards() {
    for (mode, cli_auto) in [
        (SessionModeChoice::Manual, false),
        (SessionModeChoice::Auto, false),
        (SessionModeChoice::Auto, true),
    ] {
        let provider = || match cli_auto {
            true => RecordingProvider::cli_auto(),
            false => RecordingProvider::default(),
        };
        let mut local = Bench::new("gate-facts-local", mode, provider(), false, false);
        let mut remote = Bench::new("gate-facts-remote", mode, provider(), true, true);
        remote.provider.answer_fresh_from_facts();
        if cli_auto {
            for bench in [&mut local, &mut remote] {
                bench.provider.queue(AgentDomainEvent::CliPermissionMode {
                    reported: "auto".into(),
                });
            }
        }
        // The server's facts are computed for the remote tab's own root, a real directory like the local one.
        let root = remote.dir.clone();
        let local_verdicts = verdicts(&mut local, None);
        let remote_verdicts = verdicts(&mut remote, Some(&root));
        assert_eq!(local_verdicts, remote_verdicts, "{mode:?} cli_auto={cli_auto}");
        let cards = |verdicts: &[Option<PermissionDecision>]| verdicts.iter().filter(|v| v.is_none()).count();
        assert_eq!(cards(&local_verdicts), cards(&remote_verdicts));
        assert!(cards(&remote_verdicts) > 0 && cards(&remote_verdicts) < remote_verdicts.len());
        // Every automatic answer that read the fact was checked fresh first; none that read no disk was.
        assert!(!remote.provider.fresh_checks().is_empty());
        local.shut_down();
        remote.shut_down();
    }
}

#[test]
fn a_request_waits_for_its_fact_then_cards_after_two_seconds() {
    let mut bench = Bench::remote("gate-facts-wait", SessionModeChoice::Auto);
    let input = json!({"file_path": bench.dir.join("main.rs")});
    bench.ask(request("p1", "Read", input));
    for _ in 0..5 {
        bench.pump();
    }
    assert!(!bench.card("p1"), "held, drawn as no card");
    assert_eq!(bench.answer("p1"), None);
    let left = bench
        .set
        .next_gate_fact_release()
        .expect("the window runs a tick by the wait's end");
    assert!(left <= agent::gate_facts::GATE_FACT_WAIT);
    // Its time is up.
    bench
        .set
        .hold_for_fact_since(bench.tab, "p1", Instant::now() - Duration::from_secs(3));
    assert_eq!(bench.set.next_gate_fact_release(), Some(Duration::ZERO));
    assert_eq!(bench.settle("p1"), None, "a card");
    assert!(bench.provider.fresh_checks().is_empty());
    assert_eq!(bench.set.next_gate_fact_release(), None);
    bench.shut_down();
}

#[test]
fn a_fact_that_arrives_in_time_is_used() {
    let mut bench = Bench::remote("gate-facts-arrives", SessionModeChoice::Auto);
    bench.provider.answer_fresh_from_facts();
    let input = json!({"file_path": bench.dir.join("main.rs")});
    bench.ask(request("p1", "Read", input.clone()));
    bench.pump();
    assert!(!bench.card("p1"));
    bench.provider.set_fact("p1", have(fact_at(&bench.dir, "Read", &input)));
    assert_eq!(bench.settle("p1"), Some(PermissionDecision::Allow));
    bench.shut_down();
}

/// The provider keeps a fact only under the digest it computed from the request's own bytes (`FactCache`, whose own
/// tests send a fact naming another input under the same id): such a fact reads as refused "for another request",
/// and the gate cards the request at once, with no fresh check.
#[test]
fn a_fact_whose_digest_differs_cards() {
    let mut bench = Bench::remote("gate-facts-digest", SessionModeChoice::Auto);
    let input = json!({"file_path": bench.dir.join("main.rs")});
    bench.provider.set_fact(
        "p1",
        GateFactState::Refused(agent::gate_facts::REFUSED_FOR_ANOTHER_REQUEST.into()),
    );
    bench.ask(request("p1", "Read", input));
    assert_eq!(bench.settle("p1"), None);
    assert!(bench.provider.fresh_checks().is_empty());
    assert!(
        bench.set.next_gate_fact_release().is_none(),
        "not held: refused is final"
    );
    bench.shut_down();
}

#[test]
fn an_auto_allow_waits_for_the_fresh_check() {
    let mut bench = Bench::remote("gate-facts-fresh", SessionModeChoice::Auto);
    let input = json!({"file_path": bench.dir.join("main.rs")});
    let fact = fact_at(&bench.dir, "Read", &input);
    bench.provider.set_fact("p1", have(fact.clone()));
    bench.ask(request("p1", "Read", input));
    until("the fresh check asked", || {
        bench.pump();
        !bench.provider.fresh_checks().is_empty()
    });
    for _ in 0..3 {
        bench.pump();
    }
    assert_eq!(bench.answer("p1"), None, "nothing goes out before the check answers");
    assert!(!bench.card("p1"));
    assert!(bench.set.next_gate_fact_release().is_some());
    assert_eq!(
        bench.provider.answer_fresh_checks(FreshFact::Both {
            first: fact.clone(),
            fresh: fact
        }),
        1
    );
    assert_eq!(bench.settle("p1"), Some(PermissionDecision::Allow));
    assert_eq!(bench.provider.fresh_checks(), ["p1"]);
    bench.shut_down();
}

/// The measured attack: the agent asks to read a file in the root, a background job then points it at a file
/// outside; the server's fresh fact differs, and the tab cards. A Manual tab, which answers in-project reads itself.
#[test]
fn a_symlink_swapped_before_the_fresh_check_cards() {
    let mut bench = Bench::remote("gate-facts-swap", SessionModeChoice::Manual);
    let secret_dir = workspace("gate-facts-secret");
    let input = json!({"file_path": bench.dir.join("notes.txt")});
    let first = fact_at(&bench.dir, "Read", &input);
    assert!(!first.classification.needs_a_human());
    bench.provider.set_fact("p1", have(first.clone()));
    bench.ask(request("p1", "Read", input.clone()));
    until("the fresh check asked", || {
        bench.pump();
        !bench.provider.fresh_checks().is_empty()
    });
    std::fs::remove_file(bench.dir.join("notes.txt")).unwrap();
    std::os::unix::fs::symlink(secret_dir.join("main.rs"), bench.dir.join("notes.txt")).unwrap();
    let fresh = fact_at(&bench.dir, "Read", &input);
    assert!(fresh.classification.needs_a_human(), "{fresh:?}");
    bench.provider.answer_fresh_checks(FreshFact::Both { first, fresh });
    assert_eq!(bench.settle("p1"), None, "a card");
    assert_eq!(bench.answers("p1"), 0);
    bench.shut_down();
}

#[test]
fn a_defer_at_a_boundary_waits_for_the_fresh_check_too() {
    let mut bench = Bench::new(
        "gate-facts-defer",
        SessionModeChoice::Auto,
        RecordingProvider::cli_auto(),
        true,
        true,
    );
    bench.provider.queue(AgentDomainEvent::CliPermissionMode {
        reported: "auto".into(),
    });
    let input = json!({"command": "rm -rf target"});
    let fact = fact_at(&bench.dir, "Bash", &input);
    assert!(fact.root_is_boundary);
    bench.provider.set_fact("p1", have(fact.clone()));
    bench.ask(request("p1", "Bash", input));
    until("the fresh check asked", || {
        bench.pump();
        !bench.provider.fresh_checks().is_empty()
    });
    bench.pump();
    assert_eq!(bench.answer("p1"), None);
    bench.provider.answer_fresh_checks(FreshFact::Both {
        first: fact.clone(),
        fresh: fact,
    });
    assert_eq!(bench.settle("p1"), Some(PermissionDecision::Defer));
    bench.shut_down();
}

#[test]
fn a_lost_link_during_the_fresh_check_cards() {
    for answer in [
        FreshFact::Unavailable,
        FreshFact::Refused("the server's path check gave no fact: no longer pending".into()),
    ] {
        let mut bench = Bench::remote("gate-facts-lost-check", SessionModeChoice::Auto);
        let input = json!({"file_path": bench.dir.join("main.rs")});
        bench.provider.set_fact("p1", have(fact_at(&bench.dir, "Read", &input)));
        bench.ask(request("p1", "Read", input));
        until("the fresh check asked", || {
            bench.pump();
            !bench.provider.fresh_checks().is_empty()
        });
        bench.provider.answer_fresh_checks(answer);
        assert_eq!(bench.settle("p1"), None, "a card");
        assert_eq!(bench.answers("p1"), 0);
        bench.shut_down();
    }
}

#[test]
fn a_resent_automatic_answer_is_checked_fresh_again() {
    let mut bench = Bench::remote("gate-facts-resent", SessionModeChoice::Auto);
    bench.provider.answer_fresh_from_facts();
    let input = json!({"file_path": bench.dir.join("main.rs")});
    bench.provider.set_fact("p1", have(fact_at(&bench.dir, "Read", &input)));
    bench.provider.hold_permission_replies();
    bench.ask(request("p1", "Read", input));
    until("the answer on its way", || {
        bench.pump();
        bench.provider.held_permissions() == 1
    });
    assert_eq!(bench.provider.fresh_checks().len(), 1);
    bench
        .provider
        .set_link(Some(RemoteLinkState::Disconnected { since_unix_ms: 1 }));
    bench
        .provider
        .reply_to_held_permissions(Err(agent::ProviderError::Transport("the link broke".into())));
    until("the card while the link is down", || {
        bench.pump();
        bench.card("p1")
    });
    assert_eq!(
        bench.provider.fresh_checks().len(),
        1,
        "nothing asked while the link is down"
    );
    bench.provider.set_link(Some(RemoteLinkState::Live));
    until("the answer sent again", || {
        bench.pump();
        bench.provider.held_permissions() == 1
    });
    assert_eq!(
        bench.provider.fresh_checks().len(),
        2,
        "checked fresh again before it went"
    );
    bench.provider.reply_to_held_permissions(Ok(None));
    until("the reply read", || {
        bench.pump();
        !bench.set.get(bench.tab).unwrap().gate_answers().is_empty()
    });
    for _ in 0..3 {
        bench.pump();
    }
    assert_eq!(bench.provider.held_permissions(), 0, "sent twice in all");
    assert_eq!(bench.provider.fresh_checks().len(), 2);
    bench.shut_down();
}

/// Lost again after its second fresh check, an answer stays the person's card: it is not checked or sent a third
/// time.
#[test]
fn a_resent_answer_lost_again_is_not_sent_a_third_time() {
    let mut bench = Bench::remote("gate-facts-resent-twice", SessionModeChoice::Auto);
    bench.provider.answer_fresh_from_facts();
    let input = json!({"file_path": bench.dir.join("main.rs")});
    bench.provider.set_fact("p1", have(fact_at(&bench.dir, "Read", &input)));
    bench.provider.hold_permission_replies();
    bench.ask(request("p1", "Read", input));
    for round in 1..=2 {
        until("the answer on its way", || {
            bench.pump();
            bench.provider.held_permissions() == 1
        });
        bench
            .provider
            .set_link(Some(RemoteLinkState::Disconnected { since_unix_ms: round }));
        bench
            .provider
            .reply_to_held_permissions(Err(agent::ProviderError::Transport("the link broke".into())));
        until("the card", || {
            bench.pump();
            bench.card("p1")
        });
        bench.provider.set_link(Some(RemoteLinkState::Live));
    }
    for _ in 0..5 {
        bench.pump();
    }
    assert_eq!(bench.provider.held_permissions(), 0, "not sent a third time");
    assert_eq!(bench.provider.fresh_checks().len(), 2);
    assert!(bench.card("p1"));
    bench.shut_down();
}

#[test]
fn bypass_and_name_allowed_tools_need_no_fact() {
    let mut bench = Bench::remote("gate-facts-names", SessionModeChoice::Auto);
    // No fact is ever set: a call decided by its name alone goes out without one, and without a fresh check.
    bench.ask(request("p1", "TodoWrite", json!({"todos": []})));
    assert_eq!(bench.settle("p1"), Some(PermissionDecision::Allow));
    bench.ask(request("p2", "WebFetch", json!({"url": "https://example.org"})));
    assert_eq!(bench.settle("p2"), None, "carded by its name, at once");
    bench.mode(SessionModeChoice::Manual);
    bench.mode(SessionModeChoice::Bypass);
    bench.ask(request("p3", "Bash", json!({"command": "rm -rf target"})));
    assert_eq!(bench.settle("p3"), Some(PermissionDecision::Allow));
    assert!(bench.provider.fresh_checks().is_empty());
    bench.shut_down();
    // An Auto tab on a session running the CLI's auto mode is the exception, as at a local root: there the root's
    // boundary decides between leaving the call to the CLI and the decision by name, so a tool allowed by name waits
    // for its fact and is checked fresh before its deferral.
    let mut bench = Bench::new(
        "gate-facts-names-cli-auto",
        SessionModeChoice::Auto,
        RecordingProvider::cli_auto(),
        true,
        true,
    );
    bench.provider.queue(AgentDomainEvent::CliPermissionMode {
        reported: "auto".into(),
    });
    let input = json!({"todos": []});
    bench.ask(request("p1", "TodoWrite", input.clone()));
    for _ in 0..5 {
        bench.pump();
    }
    assert!(!bench.card("p1") && bench.answer("p1").is_none(), "held for its fact");
    assert!(bench.set.next_gate_fact_release().is_some());
    let fact = fact_at(&bench.dir, "TodoWrite", &input);
    assert!(fact.root_is_boundary);
    bench.provider.answer_fresh_from_facts();
    bench.provider.set_fact("p1", have(fact));
    assert_eq!(bench.settle("p1"), Some(PermissionDecision::Defer));
    assert_eq!(bench.provider.fresh_checks(), ["p1"]);
    bench.shut_down();
}

#[test]
fn a_provider_prompt_and_bypass_are_never_held() {
    let mut bench = Bench::remote("gate-facts-prompt", SessionModeChoice::Auto);
    bench.provider.queue(AgentDomainEvent::PermissionRequested {
        permission_id: "p1".into(),
        tool_use_id: Some("tu-p1".into()),
        tool_name: "Edit".into(),
        input: json!({"file_path": bench.dir.join(".git/config")}),
        provider_prompt: Some(agent::ProviderPrompt {
            reason: Some("a sensitive file".into()),
            ..Default::default()
        }),
    });
    until("the CLI's prompt drawn at once", || {
        bench.pump();
        bench.card("p1")
    });
    assert!(bench.set.next_gate_fact_release().is_none(), "nothing waits for a fact");
    bench.mode(SessionModeChoice::Manual);
    bench.mode(SessionModeChoice::Bypass);
    bench.ask(request("p2", "Write", json!({"file_path": "/tmp/x", "content": "x"})));
    assert_eq!(
        bench.answer("p2"),
        Some(PermissionDecision::Allow),
        "answered in the first drain"
    );
    assert!(bench.provider.fresh_checks().is_empty());
    bench.shut_down();
}

#[test]
fn a_saved_rule_allows_a_remote_bash_call_after_the_fresh_check() {
    let mut bench = Bench::remote("gate-facts-rule", SessionModeChoice::Auto);
    let input = json!({"command": "make build"});
    let fact = fact_at(&bench.dir, "Bash", &input);
    assert!(fact.classification.needs_a_human(), "{fact:?}");
    // Without a rule: a card, offered the rule a local card would be offered.
    bench.provider.set_fact("p1", have(fact.clone()));
    bench.ask(request("p1", "Bash", input.clone()));
    assert_eq!(bench.settle("p1"), None);
    until("the rule offer", || {
        bench.pump();
        bench.set.get(bench.tab).unwrap().session.rule_offers.contains_key("p1")
    });
    // With one, an allow, after the fresh check.
    bench.set.set_rules(rules());
    bench.provider.set_fact("p2", have(fact.clone()));
    bench.ask(request("p2", "Bash", input));
    until("the fresh check asked", || {
        bench.pump();
        bench.provider.fresh_checks() == ["p2"]
    });
    assert_eq!(bench.answer("p2"), None);
    bench.provider.answer_fresh_checks(FreshFact::Both {
        first: fact.clone(),
        fresh: fact,
    });
    assert_eq!(bench.settle("p2"), Some(PermissionDecision::Allow));
    bench.shut_down();
}

#[test]
fn the_remote_note_follows_the_servers_boundary() {
    let mut bench = Bench::remote("gate-facts-note", SessionModeChoice::Auto);
    assert_eq!(bench.note().as_deref(), Some(UNAVAILABLE), "no word on the root yet");
    bench.pump();
    bench.provider.set_root_fact(Some(RootFact {
        is_boundary: true,
        not_a_boundary: None,
    }));
    assert_eq!(bench.note(), None, "none at a boundary");
    let told = |bench: &mut Bench| -> Vec<Value> {
        let dir = bench.dir.clone();
        bench
            .set
            .pump(&dir, true)
            .view_payloads
            .iter()
            .map(|shown| serde_json::from_str::<Value>(&shown.payload).unwrap())
            .filter(|payload| payload["kind"] == "root_note")
            .collect()
    };
    assert_eq!(
        told(&mut bench),
        [json!({"kind": "root_note", "tab": bench.tab.0, "text": "", "dismissed": false})],
        "the page takes the note down"
    );
    assert!(told(&mut bench).is_empty(), "told once");
    bench.provider.set_root_fact(Some(RootFact {
        is_boundary: false,
        not_a_boundary: Some(NotABoundary::OthersCanWrite),
    }));
    let note = agent::project_boundary::note_on(NotABoundary::OthersCanWrite, HOST);
    assert_eq!(bench.note().as_deref(), Some(note.as_str()));
    assert_eq!(told(&mut bench)[0]["text"], note);
    // A refusal this build does not know the name of keeps the note that says nothing is checked.
    bench.provider.set_root_fact(Some(RootFact {
        is_boundary: false,
        not_a_boundary: None,
    }));
    assert_eq!(bench.note().as_deref(), Some(UNAVAILABLE));
    // So does a tab whose root the trust scan has not named.
    bench.provider.set_root_fact(Some(RootFact {
        is_boundary: true,
        not_a_boundary: None,
    }));
    bench.set.set_server_root(bench.tab, None);
    assert_eq!(bench.note().as_deref(), Some(UNAVAILABLE));
    bench.shut_down();
}

#[test]
fn a_helper_without_gate_facts_keeps_the_no_boundary_gate() {
    let mut bench = Bench::new(
        "gate-facts-old-helper",
        SessionModeChoice::Auto,
        RecordingProvider::default(),
        true,
        false,
    );
    let tab = bench.set.get(bench.tab).unwrap();
    assert!(matches!(tab.gate_root(&bench.dir), agent::GateRoot::RemoteWithoutFacts));
    assert_eq!(bench.note().as_deref(), Some(UNAVAILABLE));
    bench.ask(request("p1", "Read", json!({"file_path": bench.dir.join("main.rs")})));
    assert_eq!(bench.settle("p1"), None, "a card at once, as before");
    assert!(bench.set.next_gate_fact_release().is_none());
    bench.shut_down();
}

/// Facts about another root than the one the window's trust scan named on the server are not used, nor are any
/// before the scan named one: the tab keeps the gate without facts.
#[test]
fn facts_about_another_root_are_not_used() {
    let mut bench = Bench::remote("gate-facts-other-root", SessionModeChoice::Auto);
    bench.provider.serve_facts(Path::new("/srv/another"));
    let input = json!({"file_path": bench.dir.join("main.rs")});
    bench.provider.set_fact("p1", have(fact_at(&bench.dir, "Read", &input)));
    let tab = bench.set.get(bench.tab).unwrap();
    assert!(matches!(tab.gate_root(&bench.dir), agent::GateRoot::RemoteWithoutFacts));
    bench.ask(request("p1", "Read", input.clone()));
    assert_eq!(bench.settle("p1"), None);
    bench.provider.serve_facts(&bench.dir.clone());
    bench.set.set_server_root(bench.tab, None);
    let tab = bench.set.get(bench.tab).unwrap();
    assert!(matches!(tab.gate_root(&bench.dir), agent::GateRoot::RemoteWithoutFacts));
    assert!(bench.provider.fresh_checks().is_empty());
    bench.shut_down();
}

#[test]
fn a_fresh_wait_is_not_answered_twice() {
    let mut bench = Bench::remote("gate-facts-twice", SessionModeChoice::Auto);
    let input = json!({"file_path": bench.dir.join("main.rs")});
    let fact = fact_at(&bench.dir, "Read", &input);
    bench.provider.set_fact("p1", have(fact.clone()));
    bench.ask(request("p1", "Read", input.clone()));
    until("the fresh check asked", || {
        bench.pump();
        !bench.provider.fresh_checks().is_empty()
    });
    // The same request delivered again while its check is out starts no second check.
    bench.provider.queue(request("p1", "Read", input));
    for _ in 0..5 {
        bench.pump();
    }
    assert_eq!(bench.provider.fresh_checks(), ["p1"]);
    assert_eq!(bench.answers("p1"), 0);
    bench.provider.answer_fresh_checks(FreshFact::Both {
        first: fact.clone(),
        fresh: fact,
    });
    assert_eq!(bench.settle("p1"), Some(PermissionDecision::Allow));
    for _ in 0..5 {
        bench.pump();
    }
    assert_eq!(bench.answers("p1"), 1);
    bench.shut_down();
}

#[test]
fn a_mode_change_during_the_fresh_wait_cards() {
    let mut bench = Bench::remote("gate-facts-mode", SessionModeChoice::Manual);
    let input = json!({"file_path": bench.dir.join("main.rs")});
    let fact = fact_at(&bench.dir, "Read", &input);
    bench.provider.set_fact("p1", have(fact.clone()));
    bench.ask(request("p1", "Read", input));
    until("the fresh check asked", || {
        bench.pump();
        !bench.provider.fresh_checks().is_empty()
    });
    bench.mode(SessionModeChoice::Auto);
    bench.provider.answer_fresh_checks(FreshFact::Both {
        first: fact.clone(),
        fresh: fact,
    });
    assert_eq!(bench.settle("p1"), None, "a card: the check was asked for another mode");
    bench.shut_down();
}

/// The fresh check is asked with the digest of the fact it checks, and the answer goes out only while the request's
/// id still names that request: the provider's digest for it now is another (the id was given to a later request),
/// or none (no request waits under it), and the tab cards.
#[test]
fn a_fresh_answer_for_an_id_that_names_another_request_cards() {
    for now in [Some([2; 32]), None] {
        let mut bench = Bench::remote("gate-facts-reused-id", SessionModeChoice::Auto);
        let input = json!({"file_path": bench.dir.join("main.rs")});
        let fact = fact_at(&bench.dir, "Read", &input);
        bench.provider.set_fact("p1", have_named(fact.clone(), [9; 32]));
        bench.ask(request("p1", "Read", input));
        until("the fresh check asked", || {
            bench.pump();
            !bench.provider.fresh_checks().is_empty()
        });
        assert_eq!(
            bench.provider.fresh_check_digests(),
            [[9; 32]],
            "asked about the fact's own request"
        );
        bench.provider.set_fact_digest("p1", now);
        bench.provider.answer_fresh_checks(FreshFact::Both {
            first: fact.clone(),
            fresh: fact,
        });
        assert_eq!(bench.settle("p1"), None, "a card ({now:?})");
        assert_eq!(bench.answers("p1"), 0);
        bench.shut_down();
    }
}

/// The server's first fact must be the one the answer was planned from, even when it gives the same decision: here
/// both allow the read, for different reasons.
#[test]
fn a_fresh_check_whose_first_fact_is_another_cards() {
    let mut bench = Bench::remote("gate-facts-other-first", SessionModeChoice::Auto);
    let input = json!({"file_path": bench.dir.join("main.rs")});
    let fact = fact_at(&bench.dir, "Read", &input);
    let other = GateFact {
        classification: fact_at(&bench.dir, "Glob", &json!({"pattern": "*.rs"})).classification,
        ..fact.clone()
    };
    assert!(!other.classification.needs_a_human() && other != fact);
    bench.provider.set_fact("p1", have(fact.clone()));
    bench.ask(request("p1", "Read", input));
    until("the fresh check asked", || {
        bench.pump();
        !bench.provider.fresh_checks().is_empty()
    });
    bench.provider.answer_fresh_checks(FreshFact::Both {
        first: other,
        fresh: fact,
    });
    assert_eq!(bench.settle("p1"), None, "a card");
    assert_eq!(bench.answers("p1"), 0);
    bench.shut_down();
}

/// Once the check is over the decision is made again with the server's first fact under the rules as they are then:
/// a saved rule taken away during the check leaves the planned allow resting on nothing, whatever the fresh fact.
#[test]
fn a_rule_taken_away_during_the_fresh_check_cards() {
    let mut bench = Bench::remote("gate-facts-rule-gone", SessionModeChoice::Auto);
    bench.set.set_rules(rules());
    let input = json!({"command": "make build"});
    let fact = fact_at(&bench.dir, "Bash", &input);
    assert!(fact.classification.needs_a_human(), "{fact:?}");
    bench.provider.set_fact("p1", have(fact.clone()));
    bench.ask(request("p1", "Bash", input));
    until("the fresh check asked", || {
        bench.pump();
        !bench.provider.fresh_checks().is_empty()
    });
    bench.set.set_rules(agent::PrefixRules::default());
    // A fresh fact that would allow the command by itself.
    let fresh = GateFact {
        classification: fact_at(&bench.dir, "Bash", &json!({"command": "ls"})).classification,
        ..fact.clone()
    };
    assert!(!fresh.classification.needs_a_human());
    bench
        .provider
        .answer_fresh_checks(FreshFact::Both { first: fact, fresh });
    assert_eq!(bench.settle("p1"), None, "a card");
    assert_eq!(bench.answers("p1"), 0);
    bench.shut_down();
}

/// A lost automatic answer whose resend waits for its fresh check keeps its evidence: the agent's resolution that
/// shows it took the first answer after all is recorded as the host's answer, and nothing is sent again.
#[test]
fn a_lost_answer_taken_during_its_second_fresh_check_is_recorded() {
    let mut bench = Bench::remote("gate-facts-lost-taken", SessionModeChoice::Auto);
    let input = json!({"file_path": bench.dir.join("main.rs")});
    let fact = fact_at(&bench.dir, "Read", &input);
    bench.provider.set_fact("p1", have(fact.clone()));
    bench.provider.hold_permission_replies();
    bench.ask(request("p1", "Read", input));
    until("the fresh check asked", || {
        bench.pump();
        !bench.provider.fresh_checks().is_empty()
    });
    bench.provider.answer_fresh_checks(FreshFact::Both {
        first: fact.clone(),
        fresh: fact,
    });
    until("the answer on its way", || {
        bench.pump();
        bench.provider.held_permissions() == 1
    });
    bench
        .provider
        .set_link(Some(RemoteLinkState::Disconnected { since_unix_ms: 1 }));
    bench
        .provider
        .reply_to_held_permissions(Err(agent::ProviderError::Transport("the link broke".into())));
    until("the card while the link is down", || {
        bench.pump();
        bench.card("p1")
    });
    bench.provider.set_link(Some(RemoteLinkState::Live));
    until("the second fresh check asked", || {
        bench.pump();
        bench.provider.fresh_checks().len() == 2
    });
    // The agent had the first answer after all.
    bench.provider.queue(AgentDomainEvent::PermissionResolved {
        permission_id: "p1".into(),
        outcome: agent::PermissionOutcome::Allowed,
    });
    until("the resolution folded", || {
        bench.pump();
        !bench.pending("p1")
    });
    for _ in 0..3 {
        bench.pump();
    }
    let recorded = bench.set.get(bench.tab).unwrap().gate_answers().len();
    assert_eq!(recorded, 1, "the host's answer is recorded");
    assert_eq!(bench.provider.held_permissions(), 0, "nothing sent again");
    assert_eq!(bench.set.next_gate_fact_release(), None, "no wait left behind");
    bench.shut_down();
}

/// Entering bypass ends a request's fresh check: the request is answered in bypass at once, not drawn as a card once
/// the check is over.
#[test]
fn entering_bypass_ends_a_fresh_check() {
    let mut bench = Bench::remote("gate-facts-bypass-ends", SessionModeChoice::Auto);
    let input = json!({"file_path": bench.dir.join("main.rs")});
    bench.provider.set_fact("p1", have(fact_at(&bench.dir, "Read", &input)));
    bench.ask(request("p1", "Read", input));
    until("the fresh check asked", || {
        bench.pump();
        !bench.provider.fresh_checks().is_empty()
    });
    bench.mode(SessionModeChoice::Bypass);
    bench.pump();
    bench.pump();
    assert_eq!(
        bench.answer("p1"),
        Some(PermissionDecision::Allow),
        "answered in bypass"
    );
    assert!(!bench.card("p1"));
    assert_eq!(bench.set.next_gate_fact_release(), None);
    bench.shut_down();
}

/// A lost answer whose resend was waiting for its fresh check when the tab entered bypass goes out in bypass (here on
/// entering it, as the card it was drawn as) as its second send, so lost again it is not sent a third time.
#[test]
fn a_second_send_in_bypass_lost_again_is_not_sent_a_third_time() {
    let mut bench = Bench::remote("gate-facts-bypass-resend", SessionModeChoice::Auto);
    let input = json!({"file_path": bench.dir.join("main.rs")});
    let fact = fact_at(&bench.dir, "Read", &input);
    bench.provider.set_fact("p1", have(fact.clone()));
    bench.provider.hold_permission_replies();
    bench.ask(request("p1", "Read", input));
    until("the fresh check asked", || {
        bench.pump();
        !bench.provider.fresh_checks().is_empty()
    });
    bench.provider.answer_fresh_checks(FreshFact::Both {
        first: fact.clone(),
        fresh: fact,
    });
    for round in 1..=2 {
        until("the answer on its way", || {
            bench.pump();
            bench.provider.held_permissions() == 1
        });
        bench
            .provider
            .set_link(Some(RemoteLinkState::Disconnected { since_unix_ms: round }));
        bench
            .provider
            .reply_to_held_permissions(Err(agent::ProviderError::Transport("the link broke".into())));
        until("the card", || {
            bench.pump();
            bench.card("p1")
        });
        bench.provider.set_link(Some(RemoteLinkState::Live));
        if round == 1 {
            until("the second fresh check asked", || {
                bench.pump();
                bench.provider.fresh_checks().len() == 2
            });
            bench.mode(SessionModeChoice::Bypass);
        }
    }
    for _ in 0..5 {
        bench.pump();
    }
    assert_eq!(bench.provider.held_permissions(), 0, "not sent a third time");
    assert_eq!(bench.provider.fresh_checks().len(), 2);
    assert!(bench.card("p1"));
    bench.shut_down();
}

/// The note a page was sent when the tab was shown is what a pump compares with: one that changed before the tab's
/// first pump still reaches the page.
#[test]
fn a_note_that_changes_before_the_first_pump_reaches_the_page() {
    let mut bench = Bench::remote("gate-facts-note-shown", SessionModeChoice::Auto);
    let shown: Value =
        serde_json::from_str(&bench.set.shown_root_note(bench.tab).expect("a remote tab's note")).unwrap();
    assert_eq!(shown["text"], UNAVAILABLE);
    bench.provider.set_root_fact(Some(RootFact {
        is_boundary: true,
        not_a_boundary: None,
    }));
    let dir = bench.dir.clone();
    let told: Vec<Value> = bench
        .set
        .pump(&dir, true)
        .view_payloads
        .iter()
        .map(|shown| serde_json::from_str::<Value>(&shown.payload).unwrap())
        .filter(|payload| payload["kind"] == "root_note")
        .collect();
    assert_eq!(told.len(), 1, "{told:?}");
    assert_eq!(told[0]["text"], "", "the page takes the note down");
    bench.shut_down();
}
