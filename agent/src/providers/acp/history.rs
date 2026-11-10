//! The replay of a loaded session as past turns.
//!
//! `session/load` answers after it has replayed the whole conversation as ordinary updates, with nothing
//! that marks where one turn ends and the next begins. A user's message starts a turn and everything
//! after it, up to the next user message, belongs to it. The result is handed to the host once, to be
//! folded ahead of anything live, as a Claude resume's transcript is.

use super::translate::Fold;
use super::wire::{ChunkKind, Update};
use crate::projection::{
    AgentDomainEvent, AgentSessionProjection, HistoryNotice, HistorySource, TurnEndDetail, TurnOutcome,
};
use std::collections::VecDeque;

/// Settled turns retained from a session replay, in addition to the turn being assembled;
/// the closed final turn counts toward it.
pub const HISTORY_TURN_LIMIT: usize = 200;

#[derive(Default)]
pub struct History {
    projection: AgentSessionProjection,
    /// Sequence bounds of settled turns; the current turn is outside this queue.
    settled: VecDeque<u64>,
    omitted: usize,
    turn_start: u64,
    fold: Fold,
    turn: Option<String>,
    count: u64,
    /// The user's message being assembled from its chunks.
    pending_user: Option<String>,
    last_was_user: bool,
}

impl History {
    /// Folds one replayed update. Only messages and tool calls are past conversation; the rest of what an
    /// agent says while it loads (commands, titles, usage, modes) is not.
    pub fn apply(&mut self, update: &Update) {
        match update {
            Update::Chunk {
                kind: ChunkKind::User,
                text,
                ..
            } => {
                let Some(text) = text else { return };
                if self.last_was_user {
                    if let Some(pending) = self.pending_user.as_mut() {
                        pending.push_str(text);
                    }
                } else {
                    self.close_turn();
                    self.pending_user = Some(text.clone());
                    self.last_was_user = true;
                }
            }
            Update::Chunk { .. } | Update::ToolCall(_) | Update::ToolCallUpdate(_) => {
                self.last_was_user = false;
                let turn_id = self.ensure_turn();
                let projection = &mut self.projection;
                self.fold.apply(&turn_id, update, &mut |event| projection.apply(&event));
                // Replay never reads result_text. Keep only the assembled projection text, including
                // while a long reply is arriving; the live fold still keeps its own result as usual.
                self.fold.last_message = String::new();
            }
            _ => {}
        }
    }

    /// Opens the turn the next event belongs to, with the user's message that started it first.
    fn ensure_turn(&mut self) -> String {
        if let Some(id) = &self.turn {
            return id.clone();
        }
        self.count += 1;
        let id = format!("acp-history-{}", self.count);
        self.turn_start = self.projection.last_revision;
        if let Some(text) = self.pending_user.take() {
            self.projection.apply(&AgentDomainEvent::UserPromptSubmitted { text });
        }
        self.projection
            .apply(&AgentDomainEvent::TurnStarted { turn_id: id.clone() });
        self.turn = Some(id.clone());
        id
    }

    /// A past turn is over, whatever it last said.
    fn close_turn(&mut self) {
        let Some(turn_id) = self.turn.take() else { return };
        self.projection.apply(&AgentDomainEvent::TurnCompleted {
            turn_id,
            outcome: TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
            detail: TurnEndDetail::default(),
        });
        self.fold.clear();
        self.settled.push_back(self.turn_start);
        if self.settled.len() > HISTORY_TURN_LIMIT {
            self.settled.pop_front();
            self.omitted += 1;
            let first = *self.settled.front().expect("retained turns");
            // Only replay-generated records live here. Sequence numbers remain absolute so surviving
            // rows keep their identities and the restored/live boundary stays after the whole replay.
            self.projection.user_prompts.retain(|r| r.seq >= first);
            self.projection.transcript.retain(|r| r.seq >= first);
            self.projection.thinking.retain(|r| r.seq >= first);
            self.projection.tool_calls.retain(|r| r.seq >= first);
            self.projection.call_index = Default::default();
        }
    }

    /// One assembled seed for the bounded replay, with no retained stream deltas.
    /// ACP supplies no terminal turn markers, so the final synthetic turn closes here too.
    pub fn finish(mut self) -> Vec<AgentDomainEvent> {
        if self.pending_user.is_some() {
            self.ensure_turn();
        }
        self.close_turn();
        if self.count == 0 {
            return Vec::new();
        }
        if self.omitted > 0 {
            self.projection.history = Some(HistoryNotice {
                source: HistorySource::AcpReplay,
                restored_items: self.projection.user_prompts.len()
                    + self.projection.transcript.len()
                    + self.projection.tool_calls.len(),
                omitted_items: None,
                omitted_turns: Some(self.omitted),
                upto_seq: self.projection.last_revision,
                source_path: String::new(),
                attempted_transcript_path: None,
                fallback_reason: None,
                writer_version: None,
            });
        }
        let seed = self.projection.export_settled();
        debug_assert!(AgentSessionProjection::default().seed_fits(&seed).is_ok());
        vec![AgentDomainEvent::MoveSeeded(Box::new(seed))]
    }
}

#[cfg(test)]
mod tests {
    use super::super::wire::ToolCallFields;
    use super::*;
    use crate::projection::AgentSessionProjection;
    use serde_json::json;

    fn chunk(kind: ChunkKind, text: &str) -> Update {
        Update::Chunk {
            kind,
            text: Some(text.to_string()),
            message_id: None,
        }
    }

    fn replay_turn(history: &mut History, n: usize, chunks: usize) {
        history.apply(&chunk(ChunkKind::User, &format!("prompt {n}")));
        for _ in 0..chunks {
            history.apply(&chunk(ChunkKind::Thought, "thinking "));
            history.apply(&chunk(ChunkKind::Agent, "reply "));
        }
    }

    fn seed(history: History) -> AgentSessionProjection {
        let mut seed = AgentSessionProjection::default();
        for event in history.finish() {
            seed.apply(&event);
        }
        seed
    }

    #[test]
    fn acp_replay_keeps_only_recent_assembled_turns() {
        let mut history = History::default();
        for n in 0..3 * HISTORY_TURN_LIMIT {
            replay_turn(&mut history, n, 4);
        }
        let seed = seed(history);
        assert_eq!(seed.user_prompts.len(), HISTORY_TURN_LIMIT);
        assert_eq!(seed.transcript.len(), HISTORY_TURN_LIMIT);
        assert_eq!(seed.user_prompts[0].text, format!("prompt {}", 2 * HISTORY_TURN_LIMIT));
        assert_eq!(seed.transcript[0].text, "reply ".repeat(4));
        let notice = seed.history.expect("omission notice");
        // Turn counts must survive the assembled seed's ordinary projection fold.
        assert_eq!(notice.omitted_turns, Some(2 * HISTORY_TURN_LIMIT));
        assert_eq!(notice.source, HistorySource::AcpReplay);
    }

    /// Count held rows, boundaries, indexes and stream bookkeeping, rather than allocation sizes.
    fn stored_items(history: &History) -> usize {
        let p = &history.projection;
        let fold = serde_json::to_value(&history.fold).unwrap();
        let index = serde_json::to_value(&p.call_index).unwrap();
        p.user_prompts.len()
            + p.transcript.len()
            + p.thinking.len()
            + p.tool_calls.len()
            + p.turn_starts.len()
            + p.turn_endings.len()
            + p.notices.len()
            + history.settled.len()
            + usize::from(history.pending_user.is_some())
            + usize::from(!history.fold.last_message.is_empty())
            + fold["open"].as_array().unwrap().len()
            + fold["done"].as_array().unwrap().len()
            + fold["calls"].as_object().unwrap().len()
            + fold["missed"].as_array().unwrap().len()
            + index["first"].as_object().unwrap().len()
    }

    #[test]
    fn acp_replay_storage_plateaus_including_tool_indexes_and_chunked_text() {
        let mut history = History::default();
        let mut plateau = None;
        for n in 0..3 * HISTORY_TURN_LIMIT {
            history.apply(&chunk(ChunkKind::User, &format!("prompt {n}")));
            history.apply(&chunk(ChunkKind::Thought, "thought"));
            for _ in 0..64 {
                history.apply(&chunk(ChunkKind::Agent, "word "));
                assert!(history.fold.last_message.is_empty());
                assert!(stored_items(&history) <= 6 * (HISTORY_TURN_LIMIT + 1));
            }
            history.apply(&Update::ToolCall(
                ToolCallFields::parse(&json!({
                    "toolCallId": format!("tool-{n}"), "kind": "read", "status": "completed",
                    "rawOutput": format!("result {n}")
                }))
                .unwrap(),
            ));
            if n == HISTORY_TURN_LIMIT {
                plateau = Some(stored_items(&history));
            } else if n > HISTORY_TURN_LIMIT {
                assert_eq!(Some(stored_items(&history)), plateau);
            }
        }
        let seed = seed(history);
        assert_eq!(seed.tool_calls.len(), HISTORY_TURN_LIMIT);
        assert_eq!(
            seed.tool_calls[0].tool_use_id,
            format!("tool-{}", 2 * HISTORY_TURN_LIMIT)
        );
        assert_eq!(
            seed.tool_calls[0].turn_id,
            format!("acp-history-{}", 2 * HISTORY_TURN_LIMIT + 1)
        );
        let result = seed.tool_calls[0].result.as_ref().expect("retained tool result");
        assert!(!result.is_error);
        assert_eq!(result.content, json!(format!("result {}", 2 * HISTORY_TURN_LIMIT)));
        let first = seed.user_prompts[0].seq;
        assert!(seed
            .user_prompts
            .iter()
            .map(|r| r.seq)
            .chain(seed.transcript.iter().map(|r| r.seq))
            .chain(seed.thinking.iter().map(|r| r.seq))
            .chain(seed.tool_calls.iter().map(|r| r.seq))
            .chain(seed.turn_endings.iter().map(|r| r.seq))
            .chain(seed.turn_starts.iter().map(|r| r.seq))
            .chain(seed.notices.iter().map(|r| r.seq))
            .chain(seed.questions.iter().map(|r| r.seq))
            .all(|seq| seq >= first));
    }

    #[test]
    fn acp_replay_keeps_the_running_last_turn_outside_the_settled_limit() {
        let mut history = History::default();
        for n in 0..HISTORY_TURN_LIMIT + 7 {
            replay_turn(&mut history, n, 1);
        }
        assert_eq!(history.settled.len(), HISTORY_TURN_LIMIT);
        assert_eq!(history.projection.user_prompts.len(), HISTORY_TURN_LIMIT + 1);
        assert_eq!(history.projection.active_turn_id, history.turn);
        history.apply(&chunk(ChunkKind::Agent, "still running"));
        assert!(history
            .projection
            .transcript
            .last()
            .unwrap()
            .text
            .ends_with("still running"));
        let seed = seed(history);
        assert_eq!(seed.user_prompts.len(), HISTORY_TURN_LIMIT);
        assert!(seed.transcript.last().unwrap().text.ends_with("still running"));
        assert_eq!(seed.history.unwrap().omitted_turns, Some(7));
    }

    #[test]
    fn acp_replay_short_history_preserves_chunks_and_boundaries() {
        let mut history = History::default();
        history.apply(&chunk(ChunkKind::User, "first "));
        history.apply(&chunk(ChunkKind::User, "prompt"));
        history.apply(&chunk(ChunkKind::Agent, "one "));
        history.apply(&chunk(ChunkKind::Agent, "reply"));
        history.apply(&Update::Chunk {
            kind: ChunkKind::Agent,
            text: Some("second message".into()),
            message_id: Some("m2".into()),
        });
        history.apply(&Update::Chunk {
            kind: ChunkKind::Agent,
            text: Some("third message".into()),
            message_id: Some("m3".into()),
        });
        replay_turn(&mut history, 2, 1);
        // A trailing prompt is itself a turn even when no assistant update follows it.
        history.apply(&chunk(ChunkKind::User, "unanswered"));
        let seed = seed(history);
        assert_eq!(
            seed.user_prompts.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(),
            ["first prompt", "prompt 2", "unanswered"]
        );
        assert_eq!(
            seed.transcript.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(),
            ["one replysecond message", "third message", "reply "]
        );
        assert_eq!(seed.thinking.len(), 1);
        assert!(seed.active_turn_id.is_none());
        assert!(seed.history.is_none());
    }

    #[test]
    fn acp_replay_at_or_below_the_limit_loses_no_turns() {
        for turns in [0, 1, HISTORY_TURN_LIMIT - 1, HISTORY_TURN_LIMIT] {
            let mut history = History::default();
            for n in 0..turns {
                replay_turn(&mut history, n, 1);
            }
            let seed = seed(history);
            assert_eq!(seed.user_prompts.len(), turns);
            assert_eq!(seed.transcript.len(), turns);
            assert!(seed.history.is_none());
            for (n, prompt) in seed.user_prompts.iter().enumerate() {
                assert_eq!(prompt.text, format!("prompt {n}"));
            }
        }
    }

    #[test]
    fn acp_replay_does_not_retain_deltas_or_result_text() {
        let mut history = History::default();
        replay_turn(&mut history, 0, 1000);
        assert!(history.fold.last_message.is_empty());
        let events = history.finish();
        assert!(matches!(events.as_slice(), [AgentDomainEvent::MoveSeeded(_)]));
        let mut projection = AgentSessionProjection::default();
        projection.apply(&events[0]);
        assert_eq!(projection.transcript.len(), 1);
        assert_eq!(projection.transcript[0].text, "reply ".repeat(1000));
    }
}
