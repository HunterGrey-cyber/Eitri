//! The tab set's tests, by topic; each topic is a child file. What more than one of them uses -- the
//! doubles' constructors and the event builders -- is here.
#![cfg(test)]

use super::*;
use crate::test_providers::RecordingProvider;
use agent::{AgentConversation, AgentDomainEvent};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

mod auto_notes;
mod basics;
mod bypass;
mod chat_views;
mod cli_prompts;
mod manual_mode;
mod pacing_queue;
mod saving;
mod snapshots;
mod starts;
mod traces_drafts;
mod trust;
mod turn_review;

fn workspace(label: &str) -> PathBuf {
    agent::state_dirs::redirect_state_to_a_test_root();
    let dir = agent::state_dirs::test_workspace_dir(label);
    std::fs::write(dir.join("main.rs"), "fn main() {}").unwrap();
    dir
}

fn live(dir: &Path) -> (Arc<RecordingProvider>, AgentBackend) {
    let provider = Arc::new(RecordingProvider::default());
    let conversation =
        AgentConversation::create(provider.clone(), dir, agent::setting_sources::ProjectTrust::Untrusted).unwrap();
    (
        provider,
        AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        },
    )
}

fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn read(id: &str) -> AgentDomainEvent {
    AgentDomainEvent::PermissionRequested {
        permission_id: id.into(),
        tool_use_id: None,
        tool_name: "Read".into(),
        input: serde_json::json!({ "file_path": "main.rs" }),
        provider_prompt: None,
    }
}

/// `.git/main.rs` is a protected `Write` target that reliably produces a card. An ordinary
/// in-project write would be answered by the acceptEdits fast path, leaving queueing, attention
/// and confirmation tests waiting for a nonexistent card. Those mechanisms need an asking
/// request; the fast path has separate tests in `agent::permission_policy`. Using `.git/`
/// keeps this fixture recognizably a write.
fn write(id: &str) -> AgentDomainEvent {
    AgentDomainEvent::PermissionRequested {
        permission_id: id.into(),
        tool_use_id: None,
        tool_name: "Write".into(),
        input: serde_json::json!({ "file_path": ".git/main.rs", "content": "" }),
        provider_prompt: None,
    }
}

/// A `Read` OUTSIDE the project root: still cards under the classifier, and bypass still
/// answers it (bypass never consults the path boundary at all).
fn outside(id: &str) -> AgentDomainEvent {
    AgentDomainEvent::PermissionRequested {
        permission_id: id.into(),
        tool_use_id: None,
        tool_name: "Read".into(),
        input: serde_json::json!({ "file_path": "/etc/hostname" }),
        provider_prompt: None,
    }
}

fn set() -> TabSet {
    TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto)
}

/// Unwraps a `Confirm` plan out of `cycle_mode`/`cycle_default_mode`'s `Result<ModeCycle, _>`,
/// panicking with the actual value on anything else -- every bypass-entry test wants the plan,
/// never `Changed` or an `Err`.
fn plan(result: Result<ModeCycle, String>) -> BypassPlan {
    match result {
        Ok(ModeCycle::Confirm(plan)) => plan,
        other => panic!("expected Ok(ModeCycle::Confirm(_)), got {other:?}"),
    }
}

fn shut_down_all(set: &mut TabSet) {
    for tab in set.take_all() {
        if let Some(retired) = tab.retired {
            retired.into_backend().shutdown();
        }
    }
}

fn text_delta(turn: &str, text: &str) -> AgentDomainEvent {
    AgentDomainEvent::ContentDelta {
        turn_id: turn.into(),
        kind: agent::ContentKind::Text,
        text: text.into(),
    }
}

/// The assistant text an `events` payload carries, concatenated in order; empty for no payload
/// or any other envelope.
fn events_text(payload: Option<&str>) -> String {
    let Some(value) = payload.and_then(|p| serde_json::from_str::<serde_json::Value>(p).ok()) else {
        return String::new();
    };
    if value["kind"] != "events" {
        return String::new();
    }
    value["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "content_delta")
        .filter_map(|e| e["text"].as_str())
        .collect()
}

/// The transcript a `snapshot` payload carries, concatenated.
fn snapshot_text(payload: &str) -> String {
    let value: serde_json::Value = serde_json::from_str(payload).unwrap();
    assert_eq!(value["kind"], "snapshot", "{payload}");
    value["state"]["transcript"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["text"].as_str())
        .collect()
}

/// The `snapshot` envelope among a live tab's `active_state_payloads`.
fn the_snapshot(payloads: Vec<String>) -> String {
    payloads
        .into_iter()
        .find(|p| p.contains("\"kind\":\"snapshot\""))
        .expect("a live tab's snapshot")
}

/// Waits until the tab's projection holds `text` (folded by the ingestion thread, as the
/// provider's own stream is).
fn wait_for_fold(set: &TabSet, tab: TabId, text: &str) {
    until("the ingestion thread to fold the text", || {
        let backend = set.get(tab).unwrap().live().unwrap();
        let projection = backend.projection();
        projection.transcript.iter().any(|m| m.text.contains(text))
    });
}

fn starting(set: &mut TabSet, tab: TabId, title: Option<&str>, name: Option<&str>) -> mpsc::Sender<StartResult> {
    let (tx, result_rx) = mpsc::channel();
    set.get_mut(tab).unwrap().backend = TabBackend::Starting(PendingStart {
        request_id: "req".into(),
        result_rx,
        first_turn: None,
        resume: Some("5f0c1e2d-resumed".into()),
        resumed_title: title.map(str::to_string),
        resumed_name: name.map(str::to_string),
    });
    tx
}

type StartResult = Result<AgentBackend, BackendError>;

fn started(id: &str) -> AgentDomainEvent {
    AgentDomainEvent::TurnStarted { turn_id: id.into() }
}

fn completed(id: &str, outcome: agent::TurnOutcome) -> AgentDomainEvent {
    AgentDomainEvent::TurnCompleted {
        turn_id: id.into(),
        outcome,
        result_text: String::new(),
        stop_reason: None,
        usage: None,
        detail: Default::default(),
    }
}

/// Pumps until the pump has DELIVERED the turn state `running` for `tab`, returning every tab
/// whose turn ended meanwhile. Waiting on `turn_running()` instead races the ingestion thread:
/// the projection can be ahead of what the pump has delivered.
fn pump_until_running(set: &mut TabSet, dir: &Path, tab: TabId, running: bool) -> Vec<TabId> {
    let mut ended = Vec::new();
    until("the turn state", || {
        ended.extend(set.pump(dir, true).turn_ended);
        set.get(tab).unwrap().session.was_running == running
    });
    ended
}

/// Helpers for exercising gate answers, follow-up prompts and completion independently.
fn call_started(id: &str, name: &str, input: serde_json::Value) -> AgentDomainEvent {
    AgentDomainEvent::ToolCallStarted {
        turn_id: "t1".into(),
        tool_use_id: id.into(),
        name: name.into(),
        input,
    }
}

fn call_done(id: &str, is_error: bool) -> AgentDomainEvent {
    AgentDomainEvent::ToolCallCompleted {
        turn_id: "t1".into(),
        tool_use_id: id.into(),
        content: serde_json::json!(if is_error { "File has not been read yet." } else { "ok" }),
        is_error,
    }
}

/// Pumps until `id`'s completion was delivered, returning every payload the pumps produced.
fn pump_until_done(set: &mut TabSet, dir: &Path, id: &str) -> Vec<String> {
    let mut payloads = Vec::new();
    until("the call to complete", || {
        payloads.extend(set.pump(dir, true).active_payload);
        payloads
            .iter()
            .any(|p| p.contains(id) && p.contains("tool_call_completed"))
    });
    payloads
}

fn call_in_snapshot(set: &mut TabSet, id: &str) -> serde_json::Value {
    let snapshot: serde_json::Value = serde_json::from_str(&set.active_state_payloads()[0]).unwrap();
    snapshot["state"]["toolCalls"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["toolUseId"] == id)
        .cloned()
        .unwrap()
}

fn gate_write(permission_id: &str, tool_use_id: &str) -> AgentDomainEvent {
    AgentDomainEvent::PermissionRequested {
        permission_id: permission_id.into(),
        tool_use_id: Some(tool_use_id.into()),
        tool_name: "Write".into(),
        input: serde_json::json!({ "file_path": ".git/probe2", "content": "o3" }),
        provider_prompt: None,
    }
}

/// The CLI's own prompt for a call, as the sidecar translation produces it.
fn cli_prompt(
    permission_id: &str,
    tool_use_id: &str,
    tool_name: &str,
    input: serde_json::Value,
    ask_rule: Option<&str>,
) -> AgentDomainEvent {
    AgentDomainEvent::PermissionRequested {
        permission_id: permission_id.into(),
        tool_use_id: Some(tool_use_id.into()),
        tool_name: tool_name.into(),
        input,
        provider_prompt: Some(agent::ProviderPrompt {
            reason: Some("Claude requested permissions to edit .git/probe2 which is a sensitive file.".into()),
            description: Some(".git/probe2".into()),
            blocked_path: None,
            matched_ask_rule: ask_rule.map(|content| agent::MatchedAskRule {
                source: "projectSettings".into(),
                tool_name: tool_name.into(),
                rule_content: Some(content.into()),
            }),
            unrecognized_origin: None,
        }),
    }
}

fn resolved(permission_id: &str) -> AgentDomainEvent {
    AgentDomainEvent::PermissionResolved {
        permission_id: permission_id.into(),
        outcome: agent::PermissionOutcome::Allowed,
    }
}

/// A tab holding a live session whose Claude id is `session` (waits for the id to be adopted).
fn give_session(set: &mut TabSet, tab: TabId, dir: &Path, session: &str) -> Arc<RecordingProvider> {
    let (provider, backend) = live(dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    provider.open_session(session, dir);
    until("the session id", || {
        set.get(tab).unwrap().provider_session_id().as_deref() == Some(session)
    });
    provider
}

const PROJECT: &str = "conv-0123456789abcdef";

fn refused(message: &str) -> BackendError {
    BackendError {
        message: message.to_string(),
        benign: false,
    }
}

fn ids(saved: &SavedTabs) -> Vec<&str> {
    saved.tabs.iter().map(|t| t.provider_session_id.as_str()).collect()
}

fn planned(id: &str, mode: SessionModeChoice) -> PlannedTab {
    PlannedTab {
        agent: AgentId::Claude,
        provider_session_id: id.to_string(),
        name: None,
        title: Some(format!("title of {id}")),
        saved_mode: mode,
        label: format!("title of {id}"),
        view: None,
        turn_running: false,
        draft: None,
    }
}

fn plan_of(items: Vec<PlannedTab>, active: Option<&str>) -> RestorePlan {
    RestorePlan {
        items,
        skipped: Vec::new(),
        active_session: active.map(str::to_string),
    }
}

fn first_send(request_id: &str, typed: &str) -> DeferredStart {
    DeferredStart::FirstSend {
        request_id: request_id.to_string(),
        first_turn: FirstTurn {
            typed_by_person: true,
            wire: format!("wire: {typed}"),
            typed: typed.to_string(),
            shell_line: false,
        },
    }
}

fn resume_of(request_id: &str, session: &str) -> DeferredStart {
    DeferredStart::Resume {
        request_id: request_id.to_string(),
        provider_session_id: session.to_string(),
        resumed_title: Some("a title".into()),
        resumed_name: None,
    }
}

fn awaiting(set: &TabSet, tab: TabId) -> &AwaitingTrust {
    match &set.get(tab).unwrap().backend {
        TabBackend::AwaitingTrust(waiting) => waiting,
        _ => panic!("the tab is not awaiting trust"),
    }
}

fn view(n: u8) -> ViewId {
    ViewId::new(n).unwrap()
}

fn saved_tab_named(session: &str) -> SavedTab {
    SavedTab {
        agent: AgentId::Claude,
        conversation_id: String::new(),
        provider_session_id: session.to_string(),
        name: None,
        mode: SessionModeChoice::Auto,
        view: None,
        window: None,
        turn_running: false,
        draft: None,
    }
}
