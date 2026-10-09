//! The replay of a loaded session as past turns.
//!
//! `session/load` answers after it has replayed the whole conversation as ordinary updates, with nothing
//! that marks where one turn ends and the next begins. A user's message starts a turn and everything
//! after it, up to the next user message, belongs to it. The result is handed to the host once, to be
//! folded ahead of anything live, as a Claude resume's transcript is.

use super::translate::Fold;
use super::wire::{ChunkKind, Update};
use crate::projection::{AgentDomainEvent, TurnEndDetail, TurnOutcome};

#[derive(Default)]
pub struct History {
    events: Vec<AgentDomainEvent>,
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
                let events = &mut self.events;
                self.fold.apply(&turn_id, update, &mut |event| events.push(event));
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
        if let Some(text) = self.pending_user.take() {
            self.events.push(AgentDomainEvent::UserPromptSubmitted { text });
        }
        self.events.push(AgentDomainEvent::TurnStarted { turn_id: id.clone() });
        self.turn = Some(id.clone());
        id
    }

    /// A past turn is over, whatever it last said.
    fn close_turn(&mut self) {
        let Some(turn_id) = self.turn.take() else { return };
        self.events.push(AgentDomainEvent::TurnCompleted {
            turn_id,
            outcome: TurnOutcome::Completed,
            result_text: std::mem::take(&mut self.fold.last_message),
            stop_reason: None,
            usage: None,
            detail: TurnEndDetail::default(),
        });
        self.fold.clear();
    }

    /// The events of the whole replay, oldest first.
    pub fn finish(mut self) -> Vec<AgentDomainEvent> {
        if self.pending_user.is_some() {
            self.ensure_turn();
        }
        self.close_turn();
        self.events
    }
}
