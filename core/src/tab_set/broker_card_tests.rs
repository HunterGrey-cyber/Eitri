//! A broker card is drawn and counted and nothing else: it is in no list the tab set's own sweeps read, so the
//! paths that answer cards by themselves cannot reach one, and a move does not carry one.
#![cfg(test)]
use super::move_tests::{freeze_settled, take_up};
use super::*;
use crate::editor_tools::cards::EditorCardWire;
use crate::test_providers::RecordingProvider;
use agent::{AgentConversation, AgentDomainEvent};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const CARD: &str = "ec-0123456789abcdef-1";

fn editor_card(id: &str) -> EditorCardWire {
    EditorCardWire {
        id: id.to_owned(),
        seq: 7,
        tool: "open_file".to_owned(),
        tool_use_id: Some("toolu_open".to_owned()),
        title: "Show /etc/hosts in your editor?".to_owned(),
        body: Vec::new(),
        proposal: None,
    }
}

/// A `Write` into `.git`: a card in Manual and Auto, answered in Bypass.
fn provider_card(id: &str) -> AgentDomainEvent {
    AgentDomainEvent::PermissionRequested {
        permission_id: id.into(),
        tool_use_id: None,
        tool_name: "Write".into(),
        input: serde_json::json!({ "file_path": ".git/main.rs", "content": "" }),
        provider_prompt: None,
    }
}

fn live(label: &str) -> (PathBuf, TabSet, TabId, Arc<RecordingProvider>) {
    agent::state_dirs::redirect_state_to_a_test_root();
    let dir = agent::state_dirs::test_workspace_dir(label);
    std::fs::write(dir.join("main.rs"), "fn main() {}").unwrap();
    let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    let tab = set.active();
    let provider = Arc::new(RecordingProvider::default());
    let conversation =
        AgentConversation::create(provider.clone(), &dir, agent::setting_sources::ProjectTrust::Untrusted).unwrap();
    set.get_mut(tab).unwrap().backend = TabBackend::Live(AgentBackend::Conversation {
        agent: AgentId::Claude,
        conversation: Box::new(conversation),
    });
    (dir, set, tab, provider)
}

fn pump_until(set: &mut TabSet, dir: &Path, what: &str, done: impl Fn(&TabSet) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !done(set) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        set.pump(dir, true);
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn pending(set: &TabSet, tab: TabId) -> usize {
    set.get(tab).unwrap().attention.attention().pending
}

fn enter_bypass(set: &mut TabSet, tab: TabId) -> ConfirmOutcome {
    let plan = match set.cycle_mode(tab, ViewId::BASE) {
        Ok(ModeCycle::Confirm(plan)) => plan,
        other => panic!("expected a bypass prompt, got {other:?}"),
    };
    set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE).unwrap()
}

fn shut_down_all(set: &mut TabSet) {
    for tab in set.take_all() {
        if let Some(retired) = tab.retired {
            retired.into_backend().shutdown();
        }
    }
}

#[test]
fn waiting_cards_never_names_a_broker_card() {
    let (dir, mut set, tab, provider) = live("broker-card-waiting");
    provider.queue(provider_card("perm-1"));
    pump_until(&mut set, &dir, "the provider's card", |set| pending(set, tab) == 1);
    set.set_editor_cards(tab, vec![editor_card(CARD)]);
    assert_eq!(pending(&set, tab), 2, "the premise: both wait");
    assert_eq!(waiting_cards(set.get(tab).unwrap()), ["perm-1"]);
    assert!(
        !set.get(tab).unwrap().card_is_waiting(CARD),
        "a provider's answer cannot name it"
    );
    shut_down_all(&mut set);
}

#[test]
fn entering_bypass_leaves_a_broker_card_waiting() {
    let (dir, mut set, tab, provider) = live("broker-card-enter-bypass");
    provider.queue(provider_card("perm-1"));
    pump_until(&mut set, &dir, "the provider's card", |set| pending(set, tab) == 1);
    set.set_editor_cards(tab, vec![editor_card(CARD)]);

    assert_eq!(enter_bypass(&mut set, tab), ConfirmOutcome::Entered { approved: 1 });
    assert_eq!(provider.resolutions(), vec![("perm-1".to_string(), true)]);
    assert_eq!(pending(&set, tab), 1, "the broker's card still counts");
    assert_eq!(set.get(tab).unwrap().session.editor_cards, [editor_card(CARD)]);
    shut_down_all(&mut set);
}

#[test]
fn a_bypass_resync_leaves_a_broker_card_waiting() {
    let (dir, mut set, tab, provider) = live("broker-card-resync");
    assert_eq!(enter_bypass(&mut set, tab), ConfirmOutcome::Entered { approved: 0 });
    set.set_editor_cards(tab, vec![editor_card(CARD)]);
    let mut overflow = vec![AgentDomainEvent::TurnStarted { turn_id: "t1".into() }];
    for i in 0..300 {
        overflow.push(AgentDomainEvent::ContentDelta {
            turn_id: "t1".into(),
            kind: agent::ContentKind::Text,
            text: format!("chunk {i}"),
        });
    }
    overflow.push(provider_card("lost1"));
    provider.queue_all(overflow);
    let deadline = Instant::now() + Duration::from_secs(5);
    let snapshot = loop {
        assert!(Instant::now() < deadline, "timed out waiting for the resync");
        let out = set.pump(&dir, true);
        if let Some(payload) = out.active_payload {
            let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
            if value["kind"] == "snapshot" && value["state"]["pendingPermissions"].is_array() {
                break value;
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    pump_until(&mut set, &dir, "the swept card", |set| {
        set.get(tab).unwrap().session.host_answered.contains("lost1")
    });
    assert_eq!(provider.resolutions(), vec![("lost1".to_string(), true)]);
    assert_eq!(pending(&set, tab), 1, "the sweep took the provider's card only");
    assert_eq!(snapshot["state"]["editorCards"][0]["id"], CARD, "{snapshot}");
    assert_eq!(set.get(tab).unwrap().session.editor_cards, [editor_card(CARD)]);
    shut_down_all(&mut set);
}

#[test]
fn the_tray_counts_a_broker_card() {
    let (dir, mut set, tab, _provider) = live("broker-card-tray");
    set.pump(&dir, true);
    let before = set.attention().arrived;
    set.set_editor_cards(tab, vec![editor_card(CARD)]);
    let attention = set.attention();
    assert_eq!((attention.pending, attention.arrived), (1, before + 1));
    assert_eq!(
        set.oldest_card_tab(),
        Some(tab),
        "the key for the oldest card reaches it"
    );

    let frozen = freeze_settled(&mut set, tab).expect("the tab freezes");
    let record = serde_json::to_value(&frozen.tab).unwrap();
    assert_eq!(record["attention"]["cards"], serde_json::json!([]));
    assert_eq!(record["attention"]["questions"], serde_json::json!([]));
    shut_down_all(&mut set);
}

#[test]
fn a_cards_mirror_is_not_in_the_call_notes_a_move_carries() {
    let (dir, mut source, tab, provider) = live("broker-card-move");
    source.pump(&dir, true);
    let notes = source.get(tab).unwrap().notes.clone();
    source.set_editor_cards(tab, vec![editor_card(CARD)]);
    assert_eq!(source.get(tab).unwrap().notes, notes);

    let frozen = freeze_settled(&mut source, tab).expect("the tab freezes");
    let record = serde_json::to_string(&frozen.tab).unwrap();
    assert!(!record.contains(CARD), "{record}");

    let mut dest = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    let landed = take_up(frozen, &mut dest, provider.clone(), ViewId::BASE);
    assert!(dest.get(landed).unwrap().session.editor_cards.is_empty());
    assert_eq!(pending(&dest, landed), 0);
    shut_down_all(&mut dest);
}
