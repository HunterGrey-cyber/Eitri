//! Completed edits for live review, kept apart from following and client-tool lifecycle news.

use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveEdit {
    pub tab: TabId,
    pub session: String,
    pub turn_id: String,
    pub path: PathBuf,
}

/// The event's own turn id and the root turn running at that point in its batch. Record lookup
/// happens when the list is drained, after the pump has observed that batch's starts and ends.
pub(super) struct QueuedLiveEdit {
    edit: LiveEdit,
    running: Option<String>,
}

impl TabSet {
    /// Starts collecting completed edits, or drops every pending edit when switched off.
    pub fn set_live_edits(&mut self, on: bool) {
        self.live_edits = on;
        for tab in &mut self.tabs {
            tab.session.client_calls.live_edits_on = on;
            if !on {
                tab.session.client_calls.live_edits.clear();
            }
        }
    }

    /// A frozen tab keeps its edits until its move ends, as it keeps its follow paths.
    pub fn take_live_edits(&mut self) -> Vec<LiveEdit> {
        let mut records = BTreeMap::<String, BTreeSet<String>>::new();
        let mut edits = Vec::new();
        for tab in &mut self.tabs {
            if self.moving.contains(&tab.id.0) {
                continue;
            }
            for QueuedLiveEdit { mut edit, running } in std::mem::take(&mut tab.session.client_calls.live_edits) {
                let known = records.entry(edit.session.clone()).or_insert_with(|| {
                    self.turn_review
                        .as_ref()
                        .map(|review| {
                            review
                                .turns(&edit.session)
                                .into_iter()
                                .map(|record| record.turn_id)
                                .collect()
                        })
                        .unwrap_or_default()
                });
                if !known.contains(&edit.turn_id) {
                    let Some(running) = running else { continue };
                    edit.turn_id = running;
                }
                edits.push(edit);
            }
        }
        edits
    }
}

pub(super) fn collect(live: &mut LiveTab<'_>, events: &[AgentDomainEvent]) {
    if !live.following && !live.live_edits {
        return;
    }
    // The projection has already folded the whole batch. Reconstruct its root turn at each event
    // so an edit just before an ending cannot be attributed to the next turn.
    let session_id = live.backend.provider_session_id().filter(|id| !id.is_empty());
    let mut running = live.backend.projection().active_turn_id.clone();
    if let Some(first) = events.iter().find(|event| {
        matches!(
            event,
            AgentDomainEvent::TurnStarted { .. } | AgentDomainEvent::TurnCompleted { .. }
        )
    }) {
        running = match first {
            AgentDomainEvent::TurnCompleted { turn_id, .. } if turn_id != agent::NO_OPEN_TURN_ID => {
                Some(turn_id.clone())
            }
            _ => None,
        };
    }
    // Never retain the projection's guard across another backend read.
    let mut session = if events
        .iter()
        .any(|event| matches!(event, AgentDomainEvent::SessionOpened { .. }))
    {
        None
    } else {
        session_id
    };
    for outer in events {
        match outer {
            AgentDomainEvent::SessionOpened {
                provider_session_id, ..
            } => {
                session = Some(provider_session_id.clone()).filter(|id| !id.is_empty());
            }
            AgentDomainEvent::TurnStarted { turn_id } => running = Some(turn_id.clone()),
            AgentDomainEvent::TurnCompleted { turn_id, .. } if running.as_ref() == Some(turn_id) => running = None,
            AgentDomainEvent::SessionClosed { .. } | AgentDomainEvent::SessionUnavailable { .. } => running = None,
            _ => {}
        }
        let mut event = outer;
        while let AgentDomainEvent::SubagentFrame { frame, .. } = event {
            event = frame.as_ref();
        }
        let AgentDomainEvent::ToolCallCompleted {
            tool_use_id,
            turn_id,
            is_error: false,
            ..
        } = event
        else {
            continue;
        };
        let paths = live.backend.edited_by(tool_use_id);
        if live.following {
            live.session.client_calls.edited.extend(paths.iter().cloned());
        }
        if live.live_edits {
            if let Some(session) = &session {
                live.session
                    .client_calls
                    .live_edits
                    .extend(paths.into_iter().map(|path| QueuedLiveEdit {
                        edit: LiveEdit {
                            tab: live.id,
                            session: session.clone(),
                            turn_id: turn_id.clone(),
                            path,
                        },
                        running: running.clone(),
                    }));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use crate::test_providers::RecordingProvider;
    use agent::AgentConversation;
    use std::time::{Duration, Instant};

    struct Fixture {
        dir: PathBuf,
        tabs: TabSet,
        tab: TabId,
        provider: Arc<RecordingProvider>,
    }

    impl Fixture {
        fn new(bound: bool) -> Self {
            agent::state_dirs::redirect_state_to_a_test_root();
            let dir = agent::state_dirs::test_workspace_dir("live-edits");
            let provider = Arc::new(RecordingProvider::default());
            let conversation =
                AgentConversation::create(provider.clone(), &dir, agent::setting_sources::ProjectTrust::Untrusted)
                    .unwrap();
            let mut tabs = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
            tabs.install_turn_review(crate::turn_review::TurnReview::with_options(
                None,
                &dir,
                crate::turn_review::ReviewOptions::default(),
            ))
            .unwrap();
            let tab = tabs.active();
            tabs.get_mut(tab).unwrap().backend = TabBackend::Live(AgentBackend::Conversation {
                agent: AgentId::Claude,
                conversation: Box::new(conversation),
            });
            if bound {
                provider.open_session("session-live", &dir);
                tabs_test_wait(|| tabs.get(tab).unwrap().provider_session_id().is_some());
            }
            provider.queue(AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
            tabs_test_wait(|| {
                tabs.pump(&dir, true);
                tabs.get(tab)
                    .unwrap()
                    .live()
                    .unwrap()
                    .projection()
                    .active_turn_id
                    .as_deref()
                    == Some("t1")
            });
            tabs.pump(&dir, true);
            Self {
                dir,
                tabs,
                tab,
                provider,
            }
        }

        fn deliver(&mut self, events: Vec<AgentDomainEvent>, last_call: &str) {
            self.provider.queue_all(events);
            tabs_test_wait(|| {
                self.tabs.pump(&self.dir, true);
                self.tabs
                    .get(self.tab)
                    .unwrap()
                    .live()
                    .unwrap()
                    .edited_by(last_call)
                    .len()
                    > 0
                    || self
                        .tabs
                        .get(self.tab)
                        .unwrap()
                        .live()
                        .unwrap()
                        .projection()
                        .tool_calls
                        .iter()
                        .any(|call| call.tool_use_id == last_call && call.result.is_some())
            });
            self.tabs.pump(&self.dir, true);
        }

        fn assert_edit(&mut self, turn: &str, path: &str) {
            let edits = self.tabs.take_live_edits();
            assert_eq!(edits.len(), 1, "{edits:?}");
            assert_eq!(edits[0].tab, self.tab);
            assert_eq!(edits[0].session, "session-live");
            assert_eq!(edits[0].turn_id, turn);
            assert_eq!(edits[0].path, PathBuf::from(path));
            assert!(self.tabs.take_live_edits().is_empty());
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            for tab in self.tabs.take_all() {
                if let Some(retired) = tab.retired {
                    retired.into_backend().shutdown();
                }
            }
        }
    }

    fn tabs_test_wait(mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < deadline, "the queued events must reach the pump");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn edit(
        id: &str,
        turn: &str,
        name: &str,
        input: serde_json::Value,
        content: serde_json::Value,
    ) -> Vec<AgentDomainEvent> {
        vec![
            AgentDomainEvent::ToolCallStarted {
                turn_id: turn.into(),
                tool_use_id: id.into(),
                name: name.into(),
                input,
            },
            AgentDomainEvent::ToolCallCompleted {
                turn_id: turn.into(),
                tool_use_id: id.into(),
                content,
                is_error: false,
            },
        ]
    }

    fn editing(id: &str, turn: &str) -> Vec<AgentDomainEvent> {
        edit(
            id,
            turn,
            "Edit",
            serde_json::json!({"file_path":"main.rs"}),
            serde_json::json!("ok"),
        )
    }

    #[test]
    fn a_completed_edit_is_listed_with_its_session_and_turn_only_while_on() {
        let mut f = Fixture::new(true);
        f.tabs.set_live_edits(false);
        f.deliver(editing("off", "t1"), "off");
        assert!(f.tabs.take_live_edits().is_empty());
        f.tabs.set_live_edits(true);
        f.deliver(editing("on", "t1"), "on");
        f.assert_edit("t1", "main.rs");
        let mut failed = editing("failed", "t1");
        if let AgentDomainEvent::ToolCallCompleted { is_error, .. } = &mut failed[1] {
            *is_error = true;
        }
        f.deliver(failed, "failed");
        assert!(f.tabs.take_live_edits().is_empty());
        f.deliver(editing("queued", "t1"), "queued");
        f.tabs.set_live_edits(false);
        assert!(f.tabs.take_live_edits().is_empty());
    }

    #[test]
    fn a_subagent_frames_edit_carries_its_inner_turn_id() {
        let mut f = Fixture::new(true);
        f.tabs.set_live_edits(true);
        f.deliver(
            edit("parent", "t1", "Agent", serde_json::json!({}), serde_json::json!("ok")),
            "parent",
        );
        let frames = editing("child", "t1")
            .into_iter()
            .map(|frame| AgentDomainEvent::SubagentFrame {
                parent_tool_use_id: "parent".into(),
                frame: Box::new(frame),
            })
            .collect();
        f.deliver(frames, "child");
        f.assert_edit("t1", "main.rs");
        f.provider.queue_all(vec![
            AgentDomainEvent::TurnCompleted {
                turn_id: "t1".into(),
                outcome: agent::TurnOutcome::Completed,
                result_text: String::new(),
                stop_reason: None,
                usage: None,
                detail: Default::default(),
            },
            AgentDomainEvent::TurnStarted { turn_id: "t2".into() },
        ]);
        tabs_test_wait(|| {
            f.tabs.pump(&f.dir, true);
            f.tabs
                .turn_review()
                .unwrap()
                .turns("session-live")
                .iter()
                .any(|record| record.turn_id == "t1" && record.ended_ms.is_some())
        });
        f.deliver(
            edit("parent2", "t2", "Agent", serde_json::json!({}), serde_json::json!("ok")),
            "parent2",
        );
        let frames = editing("child2", "t1")
            .into_iter()
            .map(|frame| AgentDomainEvent::SubagentFrame {
                parent_tool_use_id: "parent2".into(),
                frame: Box::new(frame),
            })
            .collect();
        f.deliver(frames, "child2");
        f.assert_edit("t1", "main.rs");
    }

    #[test]
    fn an_edit_whose_turn_id_names_no_record_takes_the_running_turn() {
        let mut f = Fixture::new(true);
        f.tabs.set_live_edits(true);
        f.deliver(editing("unknown", "child-turn"), "unknown");
        f.assert_edit("t1", "main.rs");
    }

    #[test]
    fn the_last_edit_and_the_turns_end_in_one_batch_keep_the_ending_turns_id() {
        let mut f = Fixture::new(true);
        f.tabs.set_live_edits(true);
        let mut events = editing("last", "t1");
        events.push(AgentDomainEvent::TurnCompleted {
            turn_id: "t1".into(),
            outcome: agent::TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
            detail: Default::default(),
        });
        events.push(AgentDomainEvent::TurnStarted { turn_id: "t2".into() });
        f.deliver(events, "last");
        f.assert_edit("t1", "main.rs");
    }

    #[test]
    fn a_frozen_tab_keeps_its_edits_until_the_move_ends() {
        let mut f = Fixture::new(true);
        f.tabs.set_live_edits(true);
        f.deliver(editing("before-move", "t1"), "before-move");
        f.tabs.moving.insert(f.tab.0);
        assert!(f.tabs.take_live_edits().is_empty());
        f.tabs.moving.remove(&f.tab.0);
        f.assert_edit("t1", "main.rs");
    }

    #[test]
    fn a_leaving_tab_drops_its_edits() {
        let mut f = Fixture::new(true);
        f.tabs.set_live_edits(true);
        f.deliver(editing("leaving", "t1"), "leaving");
        f.tabs.take_unreported_client_calls(f.tab);
        assert!(f.tabs.take_live_edits().is_empty());
    }

    #[test]
    fn following_and_live_lists_are_independent() {
        let mut f = Fixture::new(true);
        f.tabs.set_live_edits(true);
        f.deliver(editing("live-only", "t1"), "live-only");
        assert!(f.tabs.take_follow_edits().is_empty());
        f.assert_edit("t1", "main.rs");
        f.tabs.toggle_following(f.tab);
        f.deliver(editing("both", "t1"), "both");
        assert_eq!(f.tabs.take_follow_edits(), [(f.tab, PathBuf::from("main.rs"))]);
        f.assert_edit("t1", "main.rs");
        f.deliver(editing("toggle", "t1"), "toggle");
        f.tabs.toggle_following(f.tab);
        f.assert_edit("t1", "main.rs");
        f.tabs.toggle_following(f.tab);
        f.tabs.set_live_edits(false);
        f.deliver(editing("follow-only", "t1"), "follow-only");
        assert!(f.tabs.take_live_edits().is_empty());
        assert_eq!(f.tabs.take_follow_edits(), [(f.tab, PathBuf::from("main.rs"))]);
    }

    #[test]
    fn an_applied_codex_file_change_and_a_finished_acp_edit_are_listed() {
        let mut f = Fixture::new(true);
        f.tabs.set_live_edits(true);
        f.deliver(
            edit(
                "codex",
                "t1",
                "fileChange",
                serde_json::json!({"changes":[{"path":"a.rs"}]}),
                serde_json::json!(agent::providers::codex::translate::FILE_CHANGE_APPLIED),
            ),
            "codex",
        );
        f.assert_edit("t1", "a.rs");
        f.deliver(
            edit(
                "unapplied",
                "t1",
                "fileChange",
                serde_json::json!({"changes":[{"path":"b.rs"}]}),
                serde_json::json!("declined"),
            ),
            "unapplied",
        );
        assert!(f.tabs.take_live_edits().is_empty());
        f.deliver(
            edit(
                "acp",
                "t1",
                "acp:edit",
                serde_json::json!({"diffs":[{"path":"c.rs"}]}),
                serde_json::json!("ok"),
            ),
            "acp",
        );
        f.assert_edit("t1", "c.rs");
    }

    #[test]
    fn an_edit_before_the_session_is_bound_is_not_listed() {
        let mut f = Fixture::new(false);
        f.tabs.set_live_edits(true);
        f.deliver(editing("unbound", "t1"), "unbound");
        assert!(f.tabs.take_live_edits().is_empty());
    }
}
