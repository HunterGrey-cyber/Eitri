//! Provider-neutral events this crate produces from the real Claude CLI wire protocol. UI code
//! (a future `agent-ui`) should only ever see these -- never the raw wire JSON, so a change in the
//! CLI's wire format stops at the provider adapter instead of reaching every view:
//! `wire protocol -> provider adapter -> agent::Event -> session state -> (future) UI`.

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionSource {
    /// Arrived over the per-conversation `agent-hook` Unix socket — the primary, reliable path.
    HookRelay,
    /// Arrived as an unsolicited `control_request`/`can_use_tool` — confirmed unreliable (does
    /// not fire for every tool call, e.g. `Bash`); kept only as a secondary signal, never the
    /// sole gate.
    CanUseTool,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// The CLI's own `system`/`init` line -- fired once, first, at the start of every
    /// invocation (fresh session or `--resume`).
    SessionStarted {
        session_id: String,
        model: String,
        cwd: String,
    },
    /// One `{"type": "text", "text": ...}` content block from an `assistant` message.
    ///
    /// `message_id` is that message's `message.id`. The CLI emits one `assistant` line per content
    /// block, and the blocks of one message share its id, so a different id is a different message
    /// (2026-09-25: two messages with no tool between them used to fold into one). `None` when the
    /// line carried none.
    AssistantText { text: String, message_id: Option<String> },
    /// One `{"type": "thinking", "thinking": ...}` content block. Real captured output showed
    /// this can legitimately be an empty string (extended-thinking summarization) -- callers
    /// should not assume a non-empty value.
    Thinking { text: String },
    /// One `{"type": "tool_use", ...}` content block.
    ToolStarted { id: String, name: String, input: Value },
    /// The corresponding `user`/tool_result line. `content` is left as a raw `Value` rather
    /// than assumed to be a plain string: real captured output showed it can be a bare string
    /// (`"hi"`) OR an array of typed blocks (e.g. `[{"type": "tool_reference", ...}]`) --
    /// modeling it as `Value` here means a caller decides how to render either shape rather
    /// than this crate lossily flattening one of them.
    ToolResult { id: String, content: Value, is_error: bool },
    /// The CLI's own terminal `result` line -- fired once, last, per invocation.
    TurnFinished {
        result_text: String,
        is_error: bool,
        stop_reason: Option<String>,
        total_cost_usd: f64,
        num_turns: u32,
    },
    /// Passed through as raw JSON -- v1 has no use for interpreting rate-limit detail beyond
    /// exposing that it happened; this may later gain typed fields once something actually
    /// consumes it (e.g. an agent-ui usage indicator).
    RateLimit { raw: Value },
    /// A tool-use approval request from any `PermissionSource`. `AgentSession::respond_permission`
    /// routes the answer back through the channel that delivered it.
    ///
    /// `request_id` routes the answer (`process.rs` indexes the live socket by it). `tool_use_id`
    /// identifies the gated tool call. They hold the same string on the hook-relay path but must
    /// remain separate so a change to either source does not silently break their meanings.
    ///
    /// Pass the source's `tool_use_id` verbatim, including an empty string as `Some("")`.
    /// `projection::tool_use_link` decides whether it is usable; neither producer substitutes
    /// `request_id` for it.
    ///
    /// - `HookRelay` reads the required `tool_use_id` in `hook_protocol::PreToolUseHookInput`.
    ///   A payload without it fails to parse and is denied.
    /// - `CanUseTool` reads the message's `tool_use_id`, declared required by the Agent SDK;
    ///   `None` means it was absent. This describes `wire.rs` parsing of the declared shape,
    ///   without claiming that real control requests have been observed on this path.
    PermissionRequest {
        request_id: String,
        tool_use_id: Option<String>,
        tool_name: String,
        input: Value,
        source: PermissionSource,
    },
    /// A `PreToolUse` hook call whose own payload reported a permission mode less restrictive than
    /// `default` (`classify_cli_mode` said `Ungated`). The call itself was already
    /// denied on its connection; this asks the host to close the conversation. Produced by
    /// `process::spawn_hook_listener` INSTEAD of a `PermissionRequest`, so no card can be drawn for
    /// a call the CLI would run if nobody answered it.
    UngatedCliMode { reported: String },
    /// Any `control_response` line -- acknowledges `initialize`, `interrupt`, or a caller's own
    /// `can_use_tool` answer. `agent` does not correlate these internally; see `process.rs`.
    ControlResponse {
        request_id: String,
        subtype: String,
        raw: Value,
    },
    /// Deliberately permissive catch-all: an unrecognized event never stops the stream, since a
    /// newer CLI adds event types this build cannot know. `kind` is the wire `type` field, `subtype` its
    /// `subtype` field if present (many `system` events use this to distinguish e.g.
    /// `hook_started`/`hook_response`/`init`/`thinking_tokens`).
    Unknown {
        kind: String,
        subtype: Option<String>,
        raw: Value,
    },
    /// One line of the child's stderr, verbatim. Diagnostic only -- v1 has no state effect for
    /// this (see `AgentSessionProjection::apply`), it exists purely so stderr is observable (and
    /// actually drained, so the child can never block on a full stderr pipe buffer) instead of
    /// being silently discarded.
    ProcessStderr { line: String },
    /// Fired exactly once, the first time the child process's exit is observed (via
    /// `Child::try_wait`) -- independent of, and not a replacement for, `AgentEvent::TurnFinished`
    /// (the CLI's own terminal `result` line). A process that exits WITHOUT ever having emitted
    /// `TurnFinished` (crash, bad `--resume` id, auth failure, spawn-then-die) would otherwise
    /// produce no observable signal at all that anything went wrong.
    ///
    /// `stderr_tail` carries the child's own last few stderr lines (see
    /// `process::STDERR_TAIL_CAPACITY`), verbatim and newest-last, so a caller that only ever saw
    /// `ProcessStderr` events flow past on their way to an `eprintln!` (as `session.rs` does) can
    /// still recover the real reason once the process is gone. Empty when the child produced no
    /// stderr output at all before exiting -- not a sentinel for "unknown", a true absence.
    ProcessExited { success: bool, stderr_tail: Vec<String> },
}
