//! The v2 reducer stage sitting above `agent::event::AgentEvent` (Claude-wire-specific, stays
//! crate-internal beyond `agent::session`'s own translation step): folds a stream of
//! provider-neutral `AgentDomainEvent`s into a queryable `AgentSessionProjection`, this crate's
//! sole product-state authority
//! (docs/superpowers/specs/2026-09-09-claude-runtime-provider-refactor-design.md §10.2, §10.3).
//!
//! Deliberately NOT modeled here, per that same design doc and this crate's own YAGNI discipline
//! (no field with zero current consumer): `conversation_id`/`provider`/`provider_session_id` as
//! separate identities (this phase has exactly one provider and no resumable session-id split --
//! `session_id` alone is what today's single Claude CLI process actually has), `capabilities`,
//! provider health/compatibility warning, and handoff state (all Phase 4+ concerns). Revisit this
//! struct's shape when whichever later phase actually needs one of those, not before.

use serde::Serialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentKind {
    Text,
    Thinking,
}

/// Design doc §9.6/§16.4: a turn's terminal state must distinguish these four outcomes, not
/// collapse them into one `is_error` bool. `LimitReached` has no producer anywhere in this crate
/// today -- the old Claude-CLI wire protocol carries no `terminal_reason`-equivalent signal that
/// would let `agent::session`'s translation step ever choose it. It exists in this enum so a
/// later phase (the SDK-backed runtime, which DOES expose a real `terminal_reason`) can start
/// producing it without a breaking change for every consumer that already matches on this type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnOutcome {
    Completed,
    Interrupted,
    Failed,
    LimitReached,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOutcome {
    Allowed,
    Denied,
    CancelledByInterrupt,
    CancelledBySessionClose,
    /// The provider process itself failed/crashed while this permission was pending -- the sidecar
    /// proto's `PERMISSION_OUTCOME_PROVIDER_FAILED`. No legacy-backend producer; only
    /// `ClaudeSidecarProvider`'s translation (Task 5) can produce this.
    ProviderFailed,
    /// A permission request timed out waiting for a decision -- the sidecar proto's
    /// `PERMISSION_OUTCOME_EXPIRED`. No legacy-backend producer either.
    Expired,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProjectionStatus {
    #[default]
    Starting,
    Running,
    Unavailable { reason: String },
    Closed { reason: String },
}

/// Provider-neutral (per the design doc's own naming) but Claude-only in practice this phase --
/// see this module's own header doc for what's deliberately not modeled yet.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentDomainEvent {
    SessionOpened { session_id: String, provider_session_id: String, model: String, cwd: String },
    TurnStarted { turn_id: String },
    ContentDelta { turn_id: String, kind: ContentKind, text: String },
    ToolCallStarted { turn_id: String, tool_use_id: String, name: String, input: serde_json::Value },
    ToolCallCompleted { turn_id: String, tool_use_id: String, content: serde_json::Value, is_error: bool },
    PermissionRequested { permission_id: String, tool_use_id: Option<String>, tool_name: String, input: serde_json::Value },
    PermissionResolved { permission_id: String, outcome: PermissionOutcome },
    TurnCompleted {
        turn_id: String,
        outcome: TurnOutcome,
        result_text: String,
        stop_reason: Option<String>,
        total_cost_usd: f64,
        num_turns: u32,
    },
    /// The provider process is gone unexpectedly (a real, non-zero-exit `ProcessExited`) --
    /// distinct from `SessionClosed`, which is an orderly end. No hook-relay/gRPC-crash producer
    /// exists yet in this phase; the only current producer is `agent::session`'s translation of a
    /// crashed/non-zero-exit `AgentEvent::ProcessExited`.
    SessionUnavailable { reason: String },
    SessionClosed { reason: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolCallResult {
    pub content: serde_json::Value,
    pub is_error: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolCallRecord {
    pub turn_id: String,
    pub tool_use_id: String,
    pub name: String,
    pub input: serde_json::Value,
    /// `None` until the matching `ToolCallCompleted` arrives.
    pub result: Option<ToolCallResult>,
}

/// One unanswered permission request. Deliberately carries no `PermissionSource`
/// (`HookRelay`/`CanUseTool`) -- that Claude-wire-internal routing detail stays in
/// `agent::session`'s own bookkeeping, never in this provider-neutral type.
#[derive(Debug, Clone, Serialize)]
pub struct PermissionRequestRecord {
    pub permission_id: String,
    /// `None` for a backend whose own wire protocol has no field independent of a request id to
    /// populate this from (the legacy Claude-CLI backend's `AgentEvent::PermissionRequest` --
    /// `agent/src/session.rs`'s translation site passes `None` explicitly). `Some(..)` starting
    /// with `ClaudeSidecarProvider`, whose proto `PermissionRequested` message genuinely carries
    /// one (2026-09-10, Phase 3 of the runtime/provider refactor).
    pub tool_use_id: Option<String>,
    pub tool_name: String,
    pub input: serde_json::Value,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct UsageInfo {
    pub total_cost_usd: f64,
    pub num_turns: u32,
}

/// Neovibe's sole product-state authority for one conversation (design doc §10.3). `pending_permissions`
/// is a map (not the old `Vec`) so a caller can look up/remove one specific request by id in O(1)
/// -- exactly what answering one permission card needs, and exactly what the design doc's own
/// "pending permissions map" wording asks for.
#[derive(Debug, Clone, Default, Serialize)]
pub struct AgentSessionProjection {
    pub session_id: Option<String>,
    /// The real Claude CLI session UUID -- for the legacy backend this is identical to
    /// `session_id` above (the CLI never distinguishes them); for `ClaudeSidecarProvider` this is
    /// the wire's own `SessionReady.provider_session_id`, genuinely distinct from the sidecar's
    /// internal `session_id`. Needed for `claude --resume <id>` (Phase 4, design doc §8.1) -- not
    /// modeled before this phase per this struct's own established "no field with zero current
    /// consumer" discipline.
    pub provider_session_id: Option<String>,
    pub model: Option<String>,
    pub cwd: Option<String>,
    pub status: ProjectionStatus,
    /// `Some(turn_id)` from the moment `AgentSession::send_turn` succeeds until the matching
    /// `TurnCompleted` is folded. Design doc §5.2: "同一 session 同时最多一个 active turn" -- a
    /// second `send_turn` while this is `Some` must be rejected by the caller (Task 2), not
    /// silently queued.
    pub active_turn_id: Option<String>,
    /// Plain assistant text, in arrival order -- unchanged in shape from the pre-Phase-1
    /// `AgentSessionState::transcript`. Design doc §10.3 calls for "ordered transcript items" as a
    /// single interleaved sequence with tool calls; this phase deliberately keeps the existing
    /// split transcript/tool_calls shape instead of inventing an interleaved one, since no current
    /// consumer needs true interleaving -- revisit if one does.
    pub transcript: Vec<String>,
    pub tool_calls: Vec<ToolCallRecord>,
    pub pending_permissions: HashMap<String, PermissionRequestRecord>,
    pub usage: UsageInfo,
    /// True while the last thing folded was assistant text, so the next chunk CONTINUES the same
    /// message instead of starting a new one.
    ///
    /// Load-bearing since partial streaming landed. `transcript` means "assistant messages", not
    /// "content events": with `StreamingPreference::Partial` a single 600-word reply arrives as 400+
    /// `ContentDelta`s, and pushing each as its own entry would render 400 separate message bubbles,
    /// each markdown-parsed IN ISOLATION -- so a fragment like "`neovibe_" or "**bold" is not valid
    /// standalone markdown and the formatting of every streamed reply breaks.
    ///
    /// Anything that can only occur BETWEEN assistant messages closes the run: a tool call, a
    /// permission request, a turn boundary, a session event. `Thinking` deliberately does not --
    /// it has no transcript effect and must not split the text around it.
    #[serde(skip)]
    pub assistant_message_open: bool,
    /// Strictly increasing by exactly one per `apply` call. What a WebView bridge snapshot's
    /// `throughRevision` reports, and what a later `events{fromRevision, throughRevision, ...}`
    /// batch continues from (Task 4).
    pub last_revision: u64,
}

impl AgentSessionProjection {
    /// The reducer: folds one event's effect into this projection, unconditionally bumping
    /// `last_revision` by exactly one regardless of whether the event has any other observable
    /// effect (an event with a real revision but no other effect -- e.g. a `ContentDelta` with
    /// `ContentKind::Thinking` -- is still a real, ordered occurrence a WebView bridge must be
    /// able to skip past correctly when replaying from a revision, not something invisible to the
    /// counter). Pure and side-effect-free -- no I/O -- so it's trivially testable without
    /// spawning anything.
    pub fn apply(&mut self, event: &AgentDomainEvent) {
        match event {
            AgentDomainEvent::SessionOpened { session_id, provider_session_id, model, cwd } => {
                self.session_id = Some(session_id.clone());
                self.provider_session_id = Some(provider_session_id.clone());
                self.model = Some(model.clone());
                self.cwd = Some(cwd.clone());
                self.status = ProjectionStatus::Running;
            }
            AgentDomainEvent::TurnStarted { turn_id } => {
                self.active_turn_id = Some(turn_id.clone());
                self.assistant_message_open = false;
            }
            AgentDomainEvent::ContentDelta { kind: ContentKind::Text, text, .. } => {
                match self.transcript.last_mut() {
                    Some(open) if self.assistant_message_open => open.push_str(text),
                    _ => {
                        self.transcript.push(text.clone());
                        self.assistant_message_open = true;
                    }
                }
            }
            AgentDomainEvent::ContentDelta { kind: ContentKind::Thinking, .. } => {
                // No projection effect -- mirrors the pre-Phase-1 AgentSessionState::apply's
                // identical treatment of `Thinking`. Still bumps last_revision (see fn doc).
            }
            AgentDomainEvent::ToolCallStarted { turn_id, tool_use_id, name, input } => {
                // A tool call can only happen between assistant messages, so whatever text was
                // streaming has ended; the text after it is a new message.
                self.assistant_message_open = false;
                self.tool_calls.push(ToolCallRecord {
                    turn_id: turn_id.clone(),
                    tool_use_id: tool_use_id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                    result: None,
                });
            }
            AgentDomainEvent::ToolCallCompleted { tool_use_id, content, is_error, .. } => {
                if let Some(call) = self.tool_calls.iter_mut().find(|c| &c.tool_use_id == tool_use_id) {
                    call.result = Some(ToolCallResult { content: content.clone(), is_error: *is_error });
                }
            }
            AgentDomainEvent::PermissionRequested { permission_id, tool_use_id, tool_name, input } => {
                self.assistant_message_open = false;
                self.pending_permissions.insert(
                    permission_id.clone(),
                    PermissionRequestRecord {
                        permission_id: permission_id.clone(),
                        tool_use_id: tool_use_id.clone(),
                        tool_name: tool_name.clone(),
                        input: input.clone(),
                    },
                );
            }
            AgentDomainEvent::PermissionResolved { permission_id, .. } => {
                self.pending_permissions.remove(permission_id);
            }
            AgentDomainEvent::TurnCompleted { total_cost_usd, num_turns, .. } => {
                self.active_turn_id = None;
                self.assistant_message_open = false;
                self.usage = UsageInfo { total_cost_usd: *total_cost_usd, num_turns: *num_turns };
            }
            AgentDomainEvent::SessionUnavailable { reason } => {
                self.assistant_message_open = false;
                self.status = ProjectionStatus::Unavailable { reason: reason.clone() };
            }
            AgentDomainEvent::SessionClosed { reason } => {
                self.assistant_message_open = false;
                self.status = ProjectionStatus::Closed { reason: reason.clone() };
            }
        }
        self.last_revision += 1;
    }
}
