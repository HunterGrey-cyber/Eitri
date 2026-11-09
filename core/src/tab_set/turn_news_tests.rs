//! A turn's beginning is reported to the broker once, from the tab's clock.
#![cfg(test)]
use super::*;
use crate::test_providers::RecordingProvider;
use agent::{AgentConversation, AgentDomainEvent};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

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

fn began(set: &mut TabSet) -> Vec<String> {
    set.take_client_news()
        .into_iter()
        .filter_map(|(_, news)| match news {
            ClientCallNews::TurnBegan { turn_id } => Some(turn_id),
            _ => None,
        })
        .collect()
}

/// Pumps, collecting what is reported, until `done` holds of the reports so far.
fn pump_until_told(set: &mut TabSet, dir: &Path, what: &str, done: impl Fn(&[String]) -> bool) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut told = Vec::new();
    while !done(&told) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        set.pump(dir, true);
        told.extend(began(set));
        std::thread::sleep(Duration::from_millis(5));
    }
    told
}

fn pump_a_while(set: &mut TabSet, dir: &Path) -> Vec<String> {
    let mut told = Vec::new();
    for _ in 0..20 {
        set.pump(dir, true);
        told.extend(began(set));
        std::thread::sleep(Duration::from_millis(5));
    }
    told
}

fn completed(turn: &str) -> AgentDomainEvent {
    AgentDomainEvent::TurnCompleted {
        turn_id: turn.into(),
        outcome: agent::TurnOutcome::Completed,
        result_text: String::new(),
        stop_reason: None,
        usage: None,
        detail: Default::default(),
    }
}

fn editing(id: &str, is_error: bool) -> Vec<AgentDomainEvent> {
    vec![
        AgentDomainEvent::ToolCallStarted {
            turn_id: "t1".into(),
            tool_use_id: id.into(),
            name: "Edit".into(),
            input: serde_json::json!({"file_path":"main.rs"}),
        },
        AgentDomainEvent::ToolCallCompleted {
            turn_id: "t1".into(),
            tool_use_id: id.into(),
            content: serde_json::json!("ok"),
            is_error,
        },
    ]
}

fn edit_news(set: &mut TabSet, dir: &Path, id: &str) -> Vec<PathBuf> {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut paths = Vec::new();
    loop {
        set.pump(dir, true);
        paths.extend(set.take_follow_edits().into_iter().map(|(_, path)| path));
        if set
            .active_tab()
            .live()
            .unwrap()
            .projection()
            .tool_calls
            .iter()
            .any(|c| c.tool_use_id == id && c.result.is_some())
        {
            // The projection can finish folding after this pump took its delivery. Drain that
            // delivery before changing the tab's following flag.
            set.pump(dir, true);
            paths.extend(set.take_follow_edits().into_iter().map(|(_, path)| path));
            return paths;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn follow_edits_are_independent_of_client_call_news() {
    let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    let tab = set.active();
    let calls = &mut set.get_mut(tab).unwrap().session.client_calls;
    calls.edited.push(PathBuf::from("main.rs"));
    calls.news.push(ClientCallNews::TurnInterrupted);
    assert_eq!(set.take_client_news(), [(tab, ClientCallNews::TurnInterrupted)]);
    assert_eq!(set.take_follow_edits(), [(tab, PathBuf::from("main.rs"))]);
    assert!(set.take_follow_edits().is_empty());
}

#[test]
fn a_leaving_tab_discards_unreported_follow_edits() {
    let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    let tab = set.active();
    set.get_mut(tab)
        .unwrap()
        .session
        .client_calls
        .edited
        .push(PathBuf::from("main.rs"));
    assert!(set.take_unreported_client_calls(tab).is_empty());
    assert!(set.take_follow_edits().is_empty());
}

#[test]
fn an_edit_is_news_only_while_following() {
    let (dir, mut set, tab, provider) = live("edit-news");
    assert!(!set.get(tab).unwrap().following);
    provider.queue_all(editing("off", false));
    assert!(edit_news(&mut set, &dir, "off").is_empty());
    set.get_mut(tab).unwrap().following = true;
    provider.queue_all(editing("on", false));
    assert_eq!(edit_news(&mut set, &dir, "on"), [PathBuf::from("main.rs")]);
}

#[test]
fn a_failed_edit_is_no_news() {
    let (dir, mut set, tab, provider) = live("failed-edit-news");
    set.get_mut(tab).unwrap().following = true;
    provider.queue_all(editing("failed", true));
    assert!(edit_news(&mut set, &dir, "failed").is_empty());
}

#[test]
fn a_turn_start_is_news_once() {
    let (dir, mut set, tab, provider) = live("turn-news-once");
    assert!(pump_a_while(&mut set, &dir).is_empty(), "no turn, no news");
    provider.queue(AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
    let told = pump_until_told(&mut set, &dir, "the turn's news", |told| !told.is_empty());
    assert_eq!(told, ["t1"]);
    assert!(
        pump_a_while(&mut set, &dir).is_empty(),
        "a turn running on is not news again"
    );

    provider.queue(completed("t1"));
    let deadline = Instant::now() + Duration::from_secs(60);
    while set
        .get(tab)
        .unwrap()
        .live()
        .unwrap()
        .projection()
        .active_turn_id
        .is_some()
    {
        assert!(Instant::now() < deadline, "timed out waiting for the turn to end");
        set.pump(&dir, true);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(pump_a_while(&mut set, &dir).is_empty(), "an ending is no news");

    provider.queue(AgentDomainEvent::TurnStarted { turn_id: "t2".into() });
    let told = pump_until_told(&mut set, &dir, "the next turn's news", |told| !told.is_empty());
    assert_eq!(told, ["t2"]);
    for tab in set.take_all() {
        if let Some(retired) = tab.retired {
            retired.into_backend().shutdown();
        }
    }
}

/// A resync drops the events a pump would have read, and the clock is stamped from the projection all the same.
#[test]
fn a_turn_seen_only_through_a_resync_is_news() {
    let (dir, mut set, tab, provider) = live("turn-news-resync");
    let mut flood = vec![AgentDomainEvent::TurnStarted { turn_id: "t1".into() }];
    for i in 0..300 {
        flood.push(AgentDomainEvent::ContentDelta {
            turn_id: "t1".into(),
            kind: agent::ContentKind::Text,
            text: format!("chunk {i}"),
        });
    }
    provider.queue_all(flood);
    // One hold of the lock folds the whole flood, so a projection naming the turn has overflowed too. A tick that
    // drained before the fold and read the projection after it would stamp the clock a tick ahead of the snapshot.
    let deadline = Instant::now() + Duration::from_secs(60);
    while set
        .get(tab)
        .unwrap()
        .live()
        .unwrap()
        .projection()
        .active_turn_id
        .is_none()
    {
        assert!(Instant::now() < deadline, "timed out waiting for the fold");
        std::thread::sleep(Duration::from_millis(5));
    }
    let out = set.pump(&dir, true);
    let payload = out.active_payload.expect("the tab is on screen");
    let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(value["kind"], "snapshot", "the events were delivered as a snapshot");
    assert_eq!(began(&mut set), ["t1"]);
    assert!(pump_a_while(&mut set, &dir).is_empty(), "and it is reported once");
    for tab in set.take_all() {
        if let Some(retired) = tab.retired {
            retired.into_backend().shutdown();
        }
    }
}
