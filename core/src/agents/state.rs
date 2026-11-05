use crate::agent_bridge::TabStateWire;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Card,
    Question,
    Trust,
    Failed,
    Ended,
    Running,
    Background,
    Starting,
    Done,
    Idle,
    #[default]
    New,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    Needs,
    Working,
    #[default]
    Quiet,
    Ended,
}

impl State {
    pub fn class(self) -> Class {
        match self {
            Self::Card | Self::Question | Self::Trust => Class::Needs,
            Self::Running | Self::Background | Self::Starting => Class::Working,
            Self::Done | Self::Idle | Self::New => Class::Quiet,
            Self::Failed | Self::Ended => Class::Ended,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Card => "card",
            Self::Question => "question",
            Self::Trust => "trust",
            Self::Failed => "failed",
            Self::Ended => "ended",
            Self::Running => "running",
            Self::Background => "background",
            Self::Starting => "starting",
            Self::Done => "done",
            Self::Idle => "idle",
            Self::New => "new",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "card" => Self::Card,
            "question" => Self::Question,
            "trust" => Self::Trust,
            "failed" => Self::Failed,
            "ended" => Self::Ended,
            "running" => Self::Running,
            "background" => Self::Background,
            "starting" => Self::Starting,
            "done" => Self::Done,
            "idle" => Self::Idle,
            "new" => Self::New,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndedHow {
    Closed,
    Stopped,
    Unavailable,
    StartFailed,
    Tripped,
}

#[derive(Debug, Clone)]
pub struct TabInputs {
    pub wire: TabStateWire,
    pub cards: usize,
    pub editor_cards: usize,
    pub questions: usize,
    pub unread: bool,
    pub turn_running: bool,
    pub cli_unseen_busy: bool,
    pub background_calls: u32,
    pub stopped_resumable: bool,
    pub failed_after_start: bool,
    pub backend_failed: bool,
    pub closed: bool,
}

/// Attention comes first; an ended session cannot be working even if its last turn still looks open.
pub fn derive(i: &TabInputs) -> (State, Option<EndedHow>) {
    if i.cards > 0 || i.editor_cards > 0 {
        return (State::Card, None);
    }
    if i.questions > 0 {
        return (State::Question, None);
    }
    if i.wire == TabStateWire::AwaitingTrust {
        return (State::Trust, None);
    }
    if i.backend_failed || i.wire == TabStateWire::Failed {
        return (
            State::Failed,
            Some(if i.failed_after_start {
                EndedHow::Tripped
            } else {
                EndedHow::StartFailed
            }),
        );
    }
    if i.wire == TabStateWire::Ended && !i.closed && !i.stopped_resumable {
        return (State::Failed, Some(EndedHow::Unavailable));
    }
    if i.closed || i.stopped_resumable {
        return (
            State::Ended,
            Some(if i.stopped_resumable {
                EndedHow::Stopped
            } else {
                EndedHow::Closed
            }),
        );
    }
    if i.turn_running || i.cli_unseen_busy {
        return (State::Running, None);
    }
    if i.background_calls > 0 {
        return (State::Background, None);
    }
    if i.wire == TabStateWire::Starting {
        return (State::Starting, None);
    }
    if i.wire == TabStateWire::Live && i.unread {
        return (State::Done, None);
    }
    if i.wire == TabStateWire::Live {
        return (State::Idle, None);
    }
    (State::New, None)
}
