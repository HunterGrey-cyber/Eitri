//! `/clear`: Claude Code starts a new conversation in the same process. The view keeps nothing of the
//! conversation before it and keeps what the process still holds, since the process goes on: the
//! session's identity and status, the running turn (the `/clear` turn itself ends after the reset),
//! cards still waiting for an answer with the calls they ask about, questions still waiting, the
//! count of commands left running and the roster of background tasks, the account's limits, the CLI's
//! reported mode and command list, the live band fields, and the bookkeeping held for calls not folded
//! yet.

use super::{AgentDomainEvent, AgentSessionProjection, CallIndex, QuestionState};

pub(super) fn fold(state: &mut AgentSessionProjection, event: &AgentDomainEvent) {
    #[cfg(test)]
    super::count_hook("reset::fold");
    let AgentDomainEvent::ConversationReset {
        new_conversation_id, ..
    } = event
    else {
        return;
    };
    // Every field is named, without a rest pattern: a field added to the projection does not compile
    // here until it says whether it belongs to the conversation (emptied) or to the process (kept).
    let AgentSessionProjection {
        session_id: _,
        provider_session_id,
        model: _,
        cwd: _,
        status: _,
        active_turn_id: _,
        user_prompts,
        transcript,
        tool_calls,
        pending_permissions,
        turn_endings,
        last_turn_ending,
        turn_starts,
        notices,
        api_retry: _,
        compacting: _,
        rate_limit_warning: _,
        fallback_model: _,
        effort: _,
        notification: _,
        questions,
        last_turn_end_seq,
        usage,
        assistant_message_open,
        thinking,
        thinking_open,
        last_revision: _,
        history,
        ungated_cli_mode: _,
        cli_mode_reported: _,
        policy_mismatch: _,
        account_limits: _,
        background_calls: _,
        background_tasks: _,
        available_commands: _,
        background_at_last_turn_end: _,
        denials_before_their_call: _,
        gate_inputs: _,
        changed_inputs_before_their_call: _,
        call_index,
        steer_outcomes: _,
        move_seeded: _,
        cli_unseen_busy: _,
        prompt_before_turn: _,
        running_turn_prompt_is_slash: _,
        running_turn_said_text: _,
        opening_prompt,
        running_turn_cli: _,
    } = state;
    // The id the CLI names for the new conversation, until its first `SessionOpened` states it: the
    // tab is saved under it in between, and a resume of the old id would bring back what was cleared.
    if !new_conversation_id.is_empty() {
        *provider_session_id = Some(new_conversation_id.clone());
    }
    user_prompts.clear();
    // The prompt row it pointed at is gone with the conversation.
    *opening_prompt = None;
    transcript.clear();
    // A waiting card's call stays with it: its answer and its result still land on that row.
    tool_calls.retain(|call| {
        pending_permissions
            .values()
            .any(|card| card.tool_use_id.as_deref() == Some(call.tool_use_id.as_str()))
    });
    *call_index = CallIndex::default();
    turn_endings.clear();
    *last_turn_ending = None;
    turn_starts.clear();
    notices.clear();
    questions.retain(|set| !matches!(set.state, QuestionState::Settled { .. }));
    *last_turn_end_seq = None;
    // The figure was the old conversation's context; the next report is the new one's.
    *usage = None;
    *assistant_message_open = false;
    thinking.clear();
    *thinking_open = false;
    // What was restored belonged to the old conversation.
    *history = None;
}

pub(super) fn snapshot_json(_state: &AgentSessionProjection, _snapshot: &mut serde_json::Value) {
    #[cfg(test)]
    super::count_hook("reset::snapshot_json");
}

#[cfg(test)]
mod tests {
    use crate::{AgentDomainEvent, AgentSessionProjection, ContentKind, QuestionState, TurnOutcome};
    use serde_json::json;

    fn call(turn_id: &str, tool_use_id: &str) -> AgentDomainEvent {
        AgentDomainEvent::ToolCallStarted {
            turn_id: turn_id.into(),
            tool_use_id: tool_use_id.into(),
            name: "Bash".into(),
            input: json!({ "command": "true" }),
        }
    }

    fn question(id: &str) -> AgentDomainEvent {
        AgentDomainEvent::QuestionAsked {
            interaction_id: id.into(),
            tool_use_id: None,
            questions: Vec::new(),
            unsupported_reason: None,
        }
    }

    /// A conversation with something of every kind, then `/clear` while its turn runs.
    fn cleared() -> (AgentSessionProjection, u64) {
        let mut p = AgentSessionProjection::default();
        for event in [
            AgentDomainEvent::SessionOpened {
                session_id: "verdandi-1".into(),
                provider_session_id: "sess-old".into(),
                model: "m".into(),
                cwd: "/ws".into(),
            },
            AgentDomainEvent::UserPromptSubmitted { text: "first".into() },
            AgentDomainEvent::TurnStarted { turn_id: "t1".into() },
            AgentDomainEvent::ContentDelta {
                turn_id: "t1".into(),
                kind: ContentKind::Thinking,
                text: "hmm".into(),
            },
            AgentDomainEvent::ContentDelta {
                turn_id: "t1".into(),
                kind: ContentKind::Text,
                text: "reply".into(),
            },
            call("t1", "tu-old"),
            question("q-settled"),
            AgentDomainEvent::QuestionSettled {
                interaction_id: "q-settled".into(),
                outcome: crate::QuestionOutcome::Declined,
                answers: Vec::new(),
                reason: None,
            },
            AgentDomainEvent::BackgroundCalls { running: 1 },
            AgentDomainEvent::TurnCompleted {
                turn_id: "t1".into(),
                outcome: TurnOutcome::Failed,
                result_text: String::new(),
                stop_reason: None,
                usage: None,
                detail: Default::default(),
            },
            AgentDomainEvent::SessionNotice {
                kind: crate::SessionNoticeKind::Compaction,
                level: crate::NoticeLevel::Notice,
                content: Some("compacted".into()),
                scalars: Default::default(),
                task_id: None,
                tool_use_id: None,
                task_status: None,
            },
            AgentDomainEvent::UserPromptSubmitted { text: "/clear".into() },
            AgentDomainEvent::TurnStarted { turn_id: "t2".into() },
            call("t2", "tu-card"),
            AgentDomainEvent::PermissionRequested {
                permission_id: "perm-1".into(),
                tool_use_id: Some("tu-card".into()),
                tool_name: "Bash".into(),
                input: json!({ "command": "true" }),
                provider_prompt: None,
            },
            question("q-waiting"),
        ] {
            p.apply(&event);
        }
        let before = p.last_revision;
        p.apply(&AgentDomainEvent::ConversationReset {
            new_conversation_id: "sess-new".into(),
            trigger: Some("clear".into()),
        });
        (p, before)
    }

    #[test]
    fn a_reset_empties_the_conversation_and_keeps_what_the_process_holds() {
        let (p, before) = cleared();
        assert_eq!(p.last_revision, before + 1, "one revision, as every event");
        assert!(p.user_prompts.is_empty());
        assert!(p.transcript.is_empty());
        assert!(p.thinking.is_empty() && !p.thinking_open && !p.assistant_message_open);
        assert!(p.turn_endings.is_empty() && p.last_turn_ending.is_none());
        assert!(p.notices.is_empty() && p.turn_starts.is_empty());
        assert!(p.history.is_none() && p.usage.is_none());

        assert_eq!(p.provider_session_id.as_deref(), Some("sess-new"));
        assert_eq!(p.session_id.as_deref(), Some("verdandi-1"));
        assert_eq!(p.active_turn_id.as_deref(), Some("t2"), "the /clear turn still runs");
        assert_eq!(p.background_calls, 1, "the commands left running are the process's");
        assert!(
            p.pending_permissions.contains_key("perm-1"),
            "the waiting card survives"
        );
        let calls: Vec<&str> = p.tool_calls.iter().map(|c| c.tool_use_id.as_str()).collect();
        assert_eq!(calls, ["tu-card"], "only the waiting card's call stays");
        let questions: Vec<(&str, bool)> = p
            .questions
            .iter()
            .map(|q| {
                (
                    q.interaction_id.as_str(),
                    matches!(q.state, QuestionState::Settled { .. }),
                )
            })
            .collect();
        assert_eq!(questions, [("q-waiting", false)]);
    }

    /// The card's call is still found by its id after the list was cut, so its result lands on it.
    #[test]
    fn the_waiting_cards_call_takes_its_result_after_the_reset() {
        let (mut p, _) = cleared();
        p.apply(&AgentDomainEvent::ToolCallCompleted {
            tool_use_id: "tu-card".into(),
            content: json!("done"),
            is_error: false,
            turn_id: "t2".into(),
        });
        assert_eq!(
            p.tool_calls[0].result.as_ref().and_then(|r| r.content.as_str()),
            Some("done")
        );
    }

    /// A later conversation folds as a fresh one does: the reply after the reset is its own message.
    #[test]
    fn the_new_conversation_starts_its_own_rows() {
        let (mut p, _) = cleared();
        for event in [
            AgentDomainEvent::TurnCompleted {
                turn_id: "t2".into(),
                outcome: TurnOutcome::Completed,
                result_text: String::new(),
                stop_reason: None,
                usage: None,
                detail: Default::default(),
            },
            AgentDomainEvent::UserPromptSubmitted { text: "after".into() },
            AgentDomainEvent::TurnStarted { turn_id: "t3".into() },
            AgentDomainEvent::ContentDelta {
                turn_id: "t3".into(),
                kind: ContentKind::Text,
                text: "new reply".into(),
            },
        ] {
            p.apply(&event);
        }
        let prompts: Vec<&str> = p.user_prompts.iter().map(|u| u.text.as_str()).collect();
        let replies: Vec<&str> = p.transcript.iter().map(|m| m.text.as_str()).collect();
        assert_eq!((prompts, replies), (vec!["after"], vec!["new reply"]));
    }
}
