//! Provider-neutral events this crate produces from the real Claude CLI wire protocol. UI code
//! (a future `agent-ui`) should only ever see these -- never the raw wire JSON, per this
//! project's own architecture decision (docs/canonical/neovibe_architecture_decisions.md §5) and the
//! early design assessment's core recommended pattern:
//! `wire protocol -> provider adapter -> agent::Event -> session state -> (future) UI`.

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionSource {
    /// Arrived over the per-conversation `agent-hook` Unix socket — the primary, reliable path.
    HookRelay,
    /// Arrived as an unsolicited `control_request`/`can_use_tool` — confirmed unreliable (does
    /// not fire for every tool call, e.g. `Bash`); kept only as a secondary signal, never the
    /// sole gate. See docs/superpowers/specs/2026-09-07-agent-v2-streaming-protocol-design.md.
    CanUseTool,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// The CLI's own `system`/`init` line -- fired once, first, at the start of every
    /// invocation (fresh session or `--resume`).
    SessionStarted { session_id: String, model: String, cwd: String },
    /// One `{"type": "text", "text": ...}` content block from an `assistant` message.
    AssistantText { text: String },
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
    /// A tool-use approval request, regardless of which underlying channel produced it -- see
    /// `PermissionSource`. Callers answer via `AgentSession::respond_permission`, which routes
    /// the answer back on whichever channel this request arrived on.
    PermissionRequest { request_id: String, tool_name: String, input: Value, source: PermissionSource },
    /// Any `control_response` line -- acknowledges `initialize`, `interrupt`, or a caller's own
    /// `can_use_tool` answer. `agent` does not correlate these internally; see `process.rs`.
    ControlResponse { request_id: String, subtype: String, raw: Value },
    /// Deliberately permissive catch-all -- see this plan's Global Constraint on never letting
    /// an unrecognized event stop the stream. `kind` is the wire `type` field, `subtype` its
    /// `subtype` field if present (many `system` events use this to distinguish e.g.
    /// `hook_started`/`hook_response`/`init`/`thinking_tokens`).
    Unknown { kind: String, subtype: Option<String>, raw: Value },
    /// One line of the child's stderr, verbatim. Diagnostic only -- v1 has no state effect for
    /// this (see `AgentSessionState::apply`), it exists purely so stderr is observable (and
    /// actually drained, so the child can never block on a full stderr pipe buffer) instead of
    /// being silently discarded.
    ProcessStderr { line: String },
    /// Fired exactly once, the first time the child process's exit is observed (via
    /// `Child::try_wait`) -- independent of, and not a replacement for, `AgentEvent::TurnFinished`
    /// (the CLI's own terminal `result` line). A process that exits WITHOUT ever having emitted
    /// `TurnFinished` (crash, bad `--resume` id, auth failure, spawn-then-die) would otherwise
    /// produce no observable signal at all that anything went wrong.
    ProcessExited { success: bool },
}
