//! Read-only rows from the same facts the tab strip and the displayed cards use.

use super::{TabBackend, TabSet};
use crate::agent_bridge::TabStateWire;
use crate::agents::wire::{cut, recut_row};
use crate::agents::{derive, Background, LabelFrom, Needs, ProjectRef, Row, State, TabInputs, WindowRef, LABEL_MAX};
use crate::tabs::TabId;
use std::collections::BTreeMap;

/// A tab's own name for its whole life: `k-` and 16 lowercase hex digits, random, minted when the tab is made and kept
/// when the tab moves to another window, is broken out into its own window, or comes back after a restart. A row's
/// `id` names the tab only in the process that holds it, and the session id appears only after the first send.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RowKey(String);

impl RowKey {
    const PREFIX: &'static str = "k-";
    const HEX_DIGITS: usize = 16;

    /// A fresh key from a random (v4) UUID, whose randomness the operating system supplies on every platform the core
    /// builds for. Its trailing half is taken, which holds only the two variant bits fixed, so no digit of the key is
    /// a constant. It is a name, not a secret.
    pub fn new() -> RowKey {
        let uuid = uuid::Uuid::new_v4();
        let bytes = &uuid.as_bytes()[Self::HEX_DIGITS / 2..];
        let mut text = String::with_capacity(Self::PREFIX.len() + Self::HEX_DIGITS);
        text.push_str(Self::PREFIX);
        for byte in bytes {
            text.push_str(&format!("{byte:02x}"));
        }
        RowKey(text)
    }

    /// `text` when it is a key's exact shape; anything else (another build's or a hand edit's) is no key.
    pub fn parse(text: &str) -> Option<RowKey> {
        let hex = text.strip_prefix(Self::PREFIX)?;
        let lower_hex = |b: u8| b.is_ascii_digit() || (b'a'..=b'f').contains(&b);
        (hex.len() == Self::HEX_DIGITS && hex.bytes().all(lower_hex)).then(|| RowKey(text.to_string()))
    }

    /// The key a saved record carries, or a fresh one when it carries none a key could be.
    pub fn from_saved(saved: Option<&str>) -> RowKey {
        saved.and_then(RowKey::parse).unwrap_or_default()
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A fresh key: a record written before tabs had keys, or by a peer that predates them, gives its tab a new one.
impl Default for RowKey {
    fn default() -> Self {
        RowKey::new()
    }
}

impl std::fmt::Display for RowKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl serde::Serialize for RowKey {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

/// Read leniently: a value that is no key's shape gives a fresh key rather than refusing the record it rides in, since
/// losing a tab's key costs a face its place for that tab while refusing the record would lose the tab.
impl<'de> serde::Deserialize<'de> for RowKey {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = <serde_json::Value as serde::Deserialize>::deserialize(deserializer)?;
        Ok(RowKey::from_saved(value.as_str()))
    }
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct AgentWatch {
    first_seen: BTreeMap<String, u64>,
    last_state: Option<State>,
    since: u64,
}

impl AgentWatch {
    pub fn observe(&mut self, ids: &[String], state: State, now_ms: u64) -> (Option<u64>, u64) {
        self.first_seen.retain(|id, _| ids.contains(id));
        for id in ids {
            self.first_seen.entry(id.clone()).or_insert(now_ms);
        }
        if self.last_state != Some(state) {
            self.last_state = Some(state);
            self.since = now_ms;
        }
        (self.first_seen.values().min().copied(), self.since)
    }
}

#[derive(Debug, Clone)]
pub struct RowContext {
    pub instance: String,
    pub project: ProjectRef,
    pub window: WindowRef,
}

impl TabSet {
    /// Whether a tab has anything a stop would end: a running turn, Claude Code working where the chat cannot show
    /// it (the rows say `running` for it too), or background work its agent left running (the live, non-ambient
    /// tasks and the calls the rows count). A tab gone, not started or closed has none.
    pub fn has_work(&self, tab: TabId) -> bool {
        let Some(backend) = self.get(tab).and_then(|tab| tab.live()) else {
            return false;
        };
        let projection = backend.projection();
        if matches!(projection.status, agent::ProjectionStatus::Closed { .. }) {
            return false;
        }
        projection.active_turn_id.is_some()
            || projection.cli_unseen_busy
            || projection.background_calls > 0
            || projection
                .background_tasks
                .rows
                .iter()
                .any(|r| r.live && !r.task.ambient)
    }

    pub fn agent_rows(&mut self, ctx: &RowContext, shown: &dyn Fn(TabId) -> bool, now_ms: u64) -> Vec<Row> {
        let mut rows = Vec::with_capacity(self.tabs.len());
        for tab in &mut self.tabs {
            let moving = self.moving.contains(&tab.id.0);
            let wire = tab.wire_state();
            let (cards, editor_cards, questions) = tab.attention.counts();
            let unread = tab.attention.attention().unread;
            // Provider accessors may take the same mutex as the projection: ask before borrowing it.
            let stopped_resumable = tab.live().is_some_and(|backend| backend.stopped_resumable());
            let mut background = Background::default();
            let mut turn_running = false;
            let mut cli_unseen_busy = false;
            let mut closed = false;
            let mut tool = None;
            if let Some(backend) = tab.live() {
                let projection = backend.projection();
                closed = matches!(projection.status, agent::ProjectionStatus::Closed { .. });
                turn_running = projection.active_turn_id.is_some();
                cli_unseen_busy = projection.cli_unseen_busy;
                background.calls = projection.background_calls;
                for task in projection
                    .background_tasks
                    .rows
                    .iter()
                    .filter(|r| r.live && !r.task.ambient)
                {
                    match task.task.kind {
                        agent::TaskKind::Shell => background.shells += 1,
                        agent::TaskKind::Monitor => background.monitors += 1,
                        agent::TaskKind::LocalAgent | agent::TaskKind::Workflow => background.agents += 1,
                        agent::TaskKind::Other => background.other += 1,
                    }
                }
                if turn_running || cli_unseen_busy {
                    tool = projection
                        .tool_calls
                        .iter()
                        .rev()
                        .find(|call| {
                            call.result.is_none()
                                && call.denied.is_none()
                                && !call.background
                                && projection.active_turn_id.as_ref().is_none_or(|id| &call.turn_id == id)
                        })
                        .map(|call| cut(&call.name, LABEL_MAX));
                }
            }
            let (mut state, mut ended_how) = derive(&TabInputs {
                wire,
                cards,
                editor_cards,
                questions,
                unread,
                turn_running,
                cli_unseen_busy,
                background_calls: background.calls,
                stopped_resumable,
                failed_after_start: tab.failed_after_start,
                backend_failed: matches!(tab.backend, TabBackend::Failed { .. }),
                closed,
            });
            // Frozen tabs keep their last displayed state while their backend crosses windows.
            if moving {
                if let Some(last) = tab.agent_watch.last_state {
                    state = last;
                    if last != State::Ended && last != State::Failed {
                        ended_how = None;
                    }
                }
            }
            if state == State::Card {
                tool = tab.attention.oldest_card_tool().map(|name| cut(name, LABEL_MAX));
            } else if state != State::Running {
                tool = None;
            }
            let mut ids = tab.attention.waiting_ids();
            if wire == TabStateWire::AwaitingTrust {
                ids.push("trust".into());
            }
            let (waiting_since, since) = tab.agent_watch.observe(&ids, state, now_ms);
            let label_from = if tab.name.as_ref().is_some_and(|s| !s.is_empty()) {
                LabelFrom::Rename
            } else if tab.session.title.as_ref().is_some_and(|s| !s.trim().is_empty()) {
                LabelFrom::Title
            } else if tab.provider_session_id().is_some_and(|s| !s.is_empty()) {
                LabelFrom::Session
            } else {
                LabelFrom::New
            };
            let id = format!("{}.{}", ctx.instance, tab.id.0);
            let mut row = Row {
                key: tab.row_key().as_str().to_string(),
                id,
                agent: tab.agent.record_name(),
                project: ctx.project.clone(),
                window: ctx.window.clone(),
                number: tab.number,
                label: cut(&tab.label_name(), LABEL_MAX),
                label_from,
                shown: shown(tab.id),
                state: state.as_str().into(),
                class: state.class(),
                since,
                needs: Needs {
                    cards: cards + editor_cards,
                    questions,
                    trust: wire == TabStateWire::AwaitingTrust,
                },
                waiting_since,
                unread,
                tool,
                background,
                queued: tab.session.queue.len(),
                mode: tab.mode.as_str().into(),
                ended_how,
                moving,
            };
            recut_row(&mut row);
            rows.push(row);
        }
        rows.sort_by_key(|row| row.number);
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_backend::{AgentBackend, AgentId, BackendKind};
    use crate::agent_bridge::SessionModeChoice;
    use crate::test_providers::RecordingProvider;
    use agent::{AgentConversation, AgentDomainEvent};
    use std::sync::Arc;

    fn set() -> TabSet {
        TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto)
    }

    fn context() -> RowContext {
        RowContext {
            instance: "0123456789abcdef".into(),
            project: ProjectRef {
                key: "fedcba9876543210".into(),
                name: "eitri".into(),
            },
            window: WindowRef {
                instance: "0123456789abcdef".into(),
                name: "1".into(),
                focused: true,
            },
        }
    }

    #[test]
    fn a_policy_answered_read_never_makes_a_card_row() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("agents-policy-read");
        std::fs::write(dir.join("main.rs"), "fn main() {}").unwrap();
        let provider = Arc::new(RecordingProvider::default());
        let conversation =
            AgentConversation::create(provider.clone(), &dir, agent::setting_sources::ProjectTrust::Untrusted).unwrap();
        conversation.fold_locally(AgentDomainEvent::PermissionRequested {
            permission_id: "read".into(),
            tool_use_id: None,
            tool_name: "Read".into(),
            input: serde_json::json!({"file_path": "main.rs"}),
            provider_prompt: None,
        });
        let mut tabs = set();
        tabs.active_tab_mut().backend = super::super::TabBackend::Live(AgentBackend::Conversation {
            agent: AgentId::Claude,
            conversation: Box::new(conversation),
        });
        tabs.pump(&dir, true);
        assert_eq!(provider.resolutions(), vec![("read".into(), true)]);
        assert!(tabs
            .active_tab()
            .live()
            .unwrap()
            .projection()
            .pending_permissions
            .contains_key("read"));
        let rows = tabs.agent_rows(&context(), &|_| false, 100);
        assert_eq!(rows[0].needs.cards, 0);
        assert_ne!(rows[0].state, "card");
        for removed in tabs.take_all() {
            if let Some(retired) = removed.retired {
                retired.into_backend().shutdown();
            }
        }
    }

    #[test]
    fn waiting_since_is_the_first_sight_and_drops_with_the_card() {
        let mut watch = AgentWatch::default();
        assert_eq!(watch.observe(&["card:a".into()], State::Card, 10), (Some(10), 10));
        assert_eq!(
            watch.observe(&["card:a".into(), "question:b".into()], State::Card, 20),
            (Some(10), 10)
        );
        assert_eq!(
            watch.observe(&["question:b".into()], State::Question, 30),
            (Some(20), 30)
        );
        assert_eq!(watch.observe(&[], State::Idle, 40), (None, 40));
        assert_eq!(watch.observe(&["card:a".into()], State::Card, 50), (Some(50), 50));

        let mut tabs = set();
        tabs.active_tab_mut().attention.resync(["same".into()]);
        let first = tabs.agent_rows(&context(), &|_| false, 100).remove(0);
        assert_eq!(first.waiting_since, Some(100));
        tabs.active_tab_mut().attention.sync_questions(["same".into()]);
        tabs.active_tab_mut().attention.sync_editor_cards(["same".into()]);
        let both = tabs.agent_rows(&context(), &|_| false, 200).remove(0);
        assert_eq!(
            both.needs,
            Needs {
                cards: 2,
                questions: 1,
                trust: false
            }
        );
        assert_eq!(both.waiting_since, Some(100));
        tabs.active_tab_mut().attention.resync([]);
        tabs.active_tab_mut().attention.sync_editor_cards([]);
        let question = tabs.agent_rows(&context(), &|_| false, 300).remove(0);
        assert_eq!(question.state, "question");
        assert_eq!(question.waiting_since, Some(200));
        tabs.active_tab_mut().attention.sync_questions([]);
        assert_eq!(tabs.agent_rows(&context(), &|_| false, 400)[0].waiting_since, None);
    }

    #[test]
    fn since_changes_only_with_the_state() {
        let mut watch = AgentWatch::default();
        assert_eq!(watch.observe(&[], State::Idle, 0), (None, 0));
        assert_eq!(watch.observe(&[], State::Idle, 20), (None, 0));
        assert_eq!(watch.observe(&[], State::Running, 30), (None, 30));
        assert_eq!(watch.observe(&[], State::Running, 40), (None, 30));
        let mut tabs = set();
        assert_eq!(tabs.agent_rows(&context(), &|_| true, 50)[0].since, 50);
        assert_eq!(tabs.agent_rows(&context(), &|_| true, 60)[0].since, 50);
        tabs.active_tab_mut().attention.sync_questions(["q".into()]);
        assert_eq!(tabs.agent_rows(&context(), &|_| true, 70)[0].since, 70);
        assert_eq!(tabs.agent_rows(&context(), &|_| true, 80)[0].since, 70);
    }

    #[test]
    fn card_tools_keep_the_oldest_delivered_name_without_request_input() {
        let mut tabs = set();
        for (id, tool) in [("old", "Read"), ("new", "Bash")] {
            tabs.active_tab_mut().attention.observe(
                &[AgentDomainEvent::PermissionRequested {
                    permission_id: id.into(),
                    tool_use_id: None,
                    tool_name: tool.into(),
                    input: serde_json::json!({"file_path": "/private/input"}),
                    provider_prompt: None,
                }],
                false,
            );
        }
        let row = tabs.agent_rows(&context(), &|_| false, 10).remove(0);
        assert_eq!(row.tool.as_deref(), Some("Read"));
        assert!(!serde_json::to_string(&row).unwrap().contains("/private/input"));
        tabs.active_tab_mut().attention.retain_pending(|id| id != "old");
        assert_eq!(
            tabs.agent_rows(&context(), &|_| false, 20)[0].tool.as_deref(),
            Some("Bash")
        );
        tabs.active_tab_mut().attention.session_ended(false);
        assert_eq!(tabs.agent_rows(&context(), &|_| false, 30)[0].tool, None);
    }

    #[test]
    fn label_is_the_tab_strip_label_cut_and_escaped() {
        let mut tabs = set();
        tabs.active_tab_mut().session.title = Some("a\u{202e}\u{200b}\n a long title which is cut".into());
        let expected = crate::agents::wire::cut(&tabs.active_tab().label_name(), crate::agents::LABEL_MAX);
        let rows = tabs.agent_rows(&context(), &|_| true, 10);
        assert_eq!(rows[0].label, expected);
        assert!(rows[0].label.contains("\\u{202e}\\u{200b}\\u{a}"));
        assert_eq!(rows[0].label_from, LabelFrom::Title);
        assert!(rows[0].shown);
    }
}
