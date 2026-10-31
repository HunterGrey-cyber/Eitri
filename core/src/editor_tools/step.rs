//! What a tool and the broker say to each other. A tool never waits: each step answers what the broker
//! should do next, and the broker comes back with the [`StepEvent`] that step asked for.

use std::path::Path;
use std::time::Duration;

use agent::setting_sources::ProjectTrust;
use agent::PermissionMode;

use super::cards::{CardAnswer, CardRequest};
use super::catalog::ToolRow;
use super::memory::Memory;
use super::way::EditorCall;
use crate::editor_context::EditorContext;

pub trait Tool: Sync {
    fn row(&self) -> &'static ToolRow;
    /// Validates the arguments. `Err` is the tool's refusal sentence; nothing has run.
    fn begin(&self, args: &serde_json::Value, cx: &ToolCx<'_>) -> Result<Box<dyn ToolRun>, ToolError>;
}

pub trait ToolRun {
    fn step(&mut self, event: StepEvent, cx: &ToolCx<'_>) -> Step;
}

#[derive(Debug)]
pub enum Step {
    /// Run this in the editor; the next event is `Answer`. In flight until it answers. A `budget` at or under
    /// `STAMP_MARGIN` sends nothing: the next event is `Answer(Err(NotAnswered))`, at once.
    Call {
        call: EditorCall,
        budget: Duration,
    },
    /// Raise a card; the call leaves flight. The next event is `Card`, after the answer and a fresh turn in the queue.
    Ask(CardRequest),
    /// Keep the tab's flight slot and come back; the next event is `Poll`.
    PollAfter(Duration),
    Done(ToolResult),
}

#[derive(Debug)]
pub enum StepEvent {
    Start,
    Answer(Result<rmpv::Value, EditorFailure>),
    Card(CardAnswer),
    Poll,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorFailure {
    /// The budget ran out, or the chunk answered `{expired = true}`: nothing ran, or it may still run and do nothing.
    NotAnswered,
    /// No editor: `RpcError::Unavailable`, a link that is down. The text is `RpcError`'s.
    Unavailable(String),
    /// The editor or window 1 refused or failed.
    Refused(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    pub text: String,
    pub is_error: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorReach {
    /// This window holds its editor.
    Own,
    /// The editor is the other window's, reached over the link.
    Link,
}

/// Whether the person can see the editor a call went to, as far as this window can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorSight {
    /// On screen, or an editor no window here lays out.
    OnScreen,
    /// This window's editor is hidden, zoomed away or tabbed away.
    OffScreen,
    /// Window 1's: from window 2 nothing can tell whether it is hidden or covered.
    OtherWindow,
}

/// A bridge call: which tab, which connection, which call on it. No `cfg`: the broker builds on every
/// platform, and only the bridge's socket host is Linux's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeCallId {
    pub tab: u64,
    pub connection: u64,
    pub call: u64,
}

/// The most `ToolResult.text` may come to, JSON-escaped: under the bridge's 1 MiB frame with room for the frame's own
/// fields, and so under the sidecar's `max_message_bytes` (4 MiB).
pub const ANSWER_MAX_ENCODED: usize = 960 * 1024;

/// The bytes `text` takes as a JSON string, its quotes included: what it costs inside a frame, where a raw
/// byte count would miss that one control byte becomes six.
pub fn encoded_len(text: &str) -> usize {
    let escaped: usize = text
        .bytes()
        .map(|byte| match byte {
            b'"' | b'\\' | 0x08 | 0x09 | 0x0a | 0x0c | 0x0d => 2,
            0..=0x1f => 6,
            _ => 1,
        })
        .sum();
    escaped + 2
}

/// What a tool may know of the call it is running.
pub struct ToolCx<'a> {
    pub mode: PermissionMode,
    pub trust: ProjectTrust,
    pub root: &'a Path,
    pub home: Option<&'a Path>,
    pub reach: EditorReach,
    pub sight: EditorSight,
    pub context: Option<&'a EditorContext>,
    pub tab: u64,
    /// The tab's provider session id.
    pub session: &'a str,
    /// The broker's call id.
    pub call: &'a str,
    /// Which call of the agent's this is; available in `Tool::begin`.
    pub agent_call: AgentCall<'a>,
    /// What a tab keeps between calls.
    pub memory: &'a Memory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentCall<'a> {
    /// A Claude tab: the model's `tool_use_id`, `None` when the CLI gave none.
    Claude(Option<&'a str>),
    /// Through the bridge: `_meta.callId` when the agent sends one (Codex), and the bridge's own request id (an ACP agent has only this).
    Bridge {
        call_id: Option<&'a str>,
        request: &'a BridgeCallId,
    },
    /// A run the host started.
    Host,
}

#[cfg(test)]
mod tests {
    use super::super::bridge_wire::MAX_FRAME;
    use super::*;

    const SIDECAR_MESSAGE_MAX: usize = 4 * 1024 * 1024;

    #[test]
    fn encoded_len_counts_what_json_escapes() {
        let every_byte: String = (0u8..=0x7f).map(char::from).collect();
        for text in [
            "",
            "plain",
            "\"quoted\" \\ back",
            "\u{8}\t\n\u{c}\r",
            "\u{1}\u{1f}\u{7f}",
            "中文 and an emoji \u{1f600}",
            every_byte.as_str(),
        ] {
            let json = serde_json::to_string(text).unwrap();
            assert_eq!(encoded_len(text), json.len(), "{text:?}");
        }
    }

    #[test]
    fn the_cap_leaves_room_in_a_frame() {
        const ROOM_FOR_A_FRAMES_OWN_FIELDS: usize = 64 * 1024;
        const {
            assert!(ANSWER_MAX_ENCODED + ROOM_FOR_A_FRAMES_OWN_FIELDS <= MAX_FRAME);
            assert!(ANSWER_MAX_ENCODED < SIDECAR_MESSAGE_MAX);
        }
    }
}
