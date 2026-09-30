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

use serde::{Deserialize, Serialize};
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

/// What the provider said about a resume. Mirrors the wire's `ResumeStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResumeStatus {
    /// The provider opened a session; `attached_provider_session_id` says which one. Necessarily the
    /// late verdict -- the provider only reports its session id at the start of a turn.
    Attached,
    /// The provider refused the requested id. Prompt, and needs no turn.
    Rejected,
    /// The provider died before it could attach, for some reason other than refusing the id.
    InitializationFailed,
}

impl ResumeStatus {
    /// Whether the requested session genuinely continued.
    ///
    /// `forked` is consulted rather than ignored because forking legitimately returns a different
    /// id. Without it, the day fork is enabled every successful fork reads as a substitution.
    pub fn attached_to_the_requested_session(self, requested: &str, attached: Option<&str>, forked: bool) -> bool {
        match self {
            ResumeStatus::Attached => forked || attached == Some(requested),
            ResumeStatus::Rejected | ResumeStatus::InitializationFailed => false,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProjectionStatus {
    #[default]
    Starting,
    Running,
    Unavailable {
        reason: String,
    },
    Closed {
        reason: String,
    },
}

/// Provider-neutral (per the design doc's own naming) but Claude-only in practice this phase --
/// see this module's own header doc for what's deliberately not modeled yet.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentDomainEvent {
    SessionOpened {
        session_id: String,
        provider_session_id: String,
        model: String,
        cwd: String,
    },
    TurnStarted {
        turn_id: String,
    },
    /// What the user sent, as they typed it.
    ///
    /// **This side's event, not the provider's.** No backend reports the prompt back: the legacy
    /// CLI's stream-json carries only what the model produced, and Verdandi's wire has no such
    /// message either. So the panel could not show what was asked -- `TranscriptMessage` is
    /// assistant text and nothing else -- and a conversation read as a monologue.
    ///
    /// `text` is the text the user typed, deliberately NOT the text that went on the wire. Wire 1
    /// composes editor context into the outgoing turn above both backends
    /// (`shell/src/agent_panel.rs`); rendering that would show a file path and a selection nobody
    /// wrote.
    UserPromptSubmitted {
        text: String,
    },
    ContentDelta {
        turn_id: String,
        kind: ContentKind,
        text: String,
    },
    /// The assistant message that was streaming is over, and the next text starts a new one --
    /// with no tool call between, which would have closed it anyway. `ContentDelta` carries no
    /// message identity, so without this two messages in a row folded into one transcript entry
    /// (`After the table.TURN-1-DONE`, the phase-3 GUI pass, 2026-09-25). Emitted by the legacy
    /// backend when a text block's `message.id` differs from the last one. The sidecar emits it
    /// too, since wave-5 Task 3, from `TextDelta.message_id` when the sidecar advertises capability
    /// `text_delta_message_id` (Verdandi 133dc03) -- see
    /// `providers::claude_sidecar::translate::MessageSplit`; absent the capability it stays silent,
    /// the same as before.
    AssistantMessageBoundary {
        turn_id: String,
    },
    ToolCallStarted {
        turn_id: String,
        tool_use_id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolCallCompleted {
        turn_id: String,
        tool_use_id: String,
        content: serde_json::Value,
        is_error: bool,
    },
    PermissionRequested {
        permission_id: String,
        tool_use_id: Option<String>,
        tool_name: String,
        input: serde_json::Value,
        /// `Some` when the CLI itself asked, after the `PreToolUse` gate had already answered (Verdandi
        /// `PERMISSION_ORIGIN_PROVIDER_PROMPT`, capability `provider_permission_prompts`, b3aa188);
        /// `None` for the gate's own request -- every legacy request, and every sidecar request
        /// whose origin is HOOK or UNSPECIFIED (an older sidecar). See [`ProviderPrompt`].
        ///
        /// Omitted from the serialized event when `None`, so a gate request reads exactly as it did
        /// before this field existed.
        #[serde(skip_serializing_if = "Option::is_none")]
        provider_prompt: Option<ProviderPrompt>,
    },
    PermissionResolved {
        permission_id: String,
        outcome: PermissionOutcome,
    },
    TurnCompleted {
        turn_id: String,
        outcome: TurnOutcome,
        result_text: String,
        stop_reason: Option<String>,
        /// `None` means the provider reported no usage for this turn -- NOT a turn that cost
        /// nothing. The distinction is load-bearing, and it is per turn: the legacy backend's own
        /// `result` line always carries a cost and a turn count (and no tokens), while the sidecar's
        /// `verdandi.claude.runtime.v1` `TurnCompleted` carries Verdandi's `TurnUsage` (capability
        /// `turn_usage`) only when the SDK's result had a usable `modelUsage` -- not on a turn the
        /// kernel synthesized, and not from a sidecar that predates the field. Before this was an
        /// `Option` the sidecar path filled in `0.0`/`0`, and the projection then stored that zero
        /// as though it were measured -- so the same UI element would have read as a real running
        /// cost on one backend and a confident, permanent "$0.00" on the other, with nothing
        /// anywhere able to tell the two apart.
        usage: Option<UsageInfo>,
    },
    /// The provider process is gone unexpectedly (a real, non-zero-exit `ProcessExited`) --
    /// distinct from `SessionClosed`, which is an orderly end. No hook-relay/gRPC-crash producer
    /// exists yet in this phase; the only current producer is `agent::session`'s translation of a
    /// crashed/non-zero-exit `AgentEvent::ProcessExited`.
    SessionUnavailable {
        reason: String,
    },
    /// The provider's verdict on a resume, reported exactly once for a session that asked for one.
    ///
    /// Replaces an inference the client could not make safely: `CreateSession` returns before the
    /// provider has tried to attach, so this side used to watch for a few seconds and treat silence
    /// as success. Measured, that had no margin -- a real rejection surfaces at ~1.7-2.3s against a
    /// 3.0s window.
    ResumeOutcome {
        requested_provider_session_id: String,
        status: ResumeStatus,
        /// Set only when `status` is `Attached`.
        attached_provider_session_id: Option<String>,
        /// True when this session asked to fork, in which case a DIFFERENT attached id is correct.
        forked: bool,
        detail: Option<String>,
    },
    SessionClosed {
        reason: String,
    },
    /// A `SetPermissionMode` the sidecar acknowledged (Verdandi `PermissionModeChanged`, capability
    /// `set_permission_mode`, 133dc03). Produced by `providers/claude_sidecar/translate.rs` when the
    /// sidecar acknowledges the `SetPermissionMode` RPC this client issues.
    ///
    /// **Since R07 (2026-09-27) this client issues no such RPC** (spec §6, D5: removed, not
    /// disabled). The pinned proto can still emit the event, so it is still decoded; one reporting
    /// `default` arrives here and is folded as a no-op, and one reporting anything less
    /// restrictive becomes `UngatedCliMode` instead (D12).
    PermissionModeChanged {
        mode: crate::PermissionMode,
        /// `SetPermissionModeResponse.permission_mode` verbatim -- `default` / `bypassPermissions`,
        /// the provider's OWN vocabulary, kept for diagnostics rather than collapsed into `mode`.
        provider_mode: String,
        /// True when entering bypass applied Verdandi's conservative floor
        /// (`PermissionModeChanged.bypass_default_deny_applied`). Neovibe states `unrestricted` on
        /// every session (`build_create_request`), so this must never happen in practice; see the
        /// translate-side match arm for what a `true` here means.
        floor_applied: bool,
    },
    /// The CLI reported a permission mode less restrictive than the `default` neovibe asks for
    /// (`crate::classify_cli_mode` said `Ungated`; spec §2.3, D12) -- most likely a project's own
    /// `permissions.defaultMode`. Under such a mode a hook that gives no answer lets the tool run,
    /// which R07 says must never be possible, so the session has to be closed.
    ///
    /// Both backends produce it: the sidecar from `SessionReady.permission_mode` or a
    /// `PermissionModeChanged`, legacy from a `PreToolUse` hook payload (whose call was already
    /// denied). `neovibe_core::tab_set::TabSet::pump` is the one place that closes the session and
    /// fails the tab.
    UngatedCliMode {
        /// The mode as the CLI named it, verbatim (`bypassPermissions`, `acceptEdits`, ...).
        reported: String,
        /// Which report it came from: `SessionReady`, `PermissionModeChanged`, or `a PreToolUse hook
        /// call`.
        detail: String,
    },
}

/// The first `UngatedCliMode` a session reported, as the projection keeps it. See
/// `AgentSessionProjection::ungated_cli_mode`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UngatedCliModeRecord {
    pub reported: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallResult {
    pub content: serde_json::Value,
    pub is_error: bool,
}

/// One assistant message, with the position it occupies among everything else this conversation
/// produced.
///
/// A bare `String` until 2026-09-15, which is what made the panel render every tool card below
/// every assistant message: the three collections reached the frontend with nothing saying how they
/// interleave. See `AgentSessionProjection::transcript` for why the order has to originate here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptMessage {
    /// Ordering key, shared with `UserPromptRecord::seq`, `ToolCallRecord::seq` and
    /// `PermissionRequestRecord::seq`. See `AgentSessionProjection::apply`.
    ///
    /// The seq of the delta that OPENED this message, never updated as it grows -- a streaming
    /// message must not keep moving below the tool call that already interrupted it.
    pub seq: u64,
    pub text: String,
}

/// One prompt the user sent, with the position it occupies among everything else.
///
/// A fourth collection rather than a role on `TranscriptMessage`, because `transcript`'s
/// coalescing (`assistant_message_open` plus `transcript.last_mut()`) is what keeps 400 streaming
/// deltas one markdown-parsed message, and a user prompt must never be a thing that walk can land
/// on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserPromptRecord {
    /// Ordering key, shared with `TranscriptMessage::seq`, `ToolCallRecord::seq` and
    /// `PermissionRequestRecord::seq`. See `AgentSessionProjection::apply`.
    pub seq: u64,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallRecord {
    /// Ordering key, shared with `UserPromptRecord::seq`, `TranscriptMessage::seq` and
    /// `PermissionRequestRecord::seq`. See `AgentSessionProjection::apply`.
    pub seq: u64,
    pub turn_id: String,
    pub tool_use_id: String,
    pub name: String,
    pub input: serde_json::Value,
    /// `None` until the matching `ToolCallCompleted` arrives.
    ///
    /// `#[serde(default)]` for the read side only (`history::store`): a stored history is a file,
    /// and a file written by an older build must still load. It is NOT `skip_serializing_if`,
    /// because the frontend distinguishes `result: null` from an absent key.
    #[serde(default)]
    pub result: Option<ToolCallResult>,
}

/// One unanswered permission request. Deliberately carries no `PermissionSource`
/// (`HookRelay`/`CanUseTool`) -- that Claude-wire-internal routing detail stays in
/// `agent::session`'s own bookkeeping, never in this provider-neutral type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionRequestRecord {
    /// Ordering key, shared with `UserPromptRecord::seq`, `TranscriptMessage::seq` and
    /// `ToolCallRecord::seq`. See `AgentSessionProjection::apply`.
    ///
    /// Needed even though a request usually has `tool_use_id` to anchor it beside its tool call:
    /// that link is absent on the legacy backend (`None`) and can arrive as `""` from the sidecar's
    /// proto3 wire, and a card with no usable link still has to land somewhere sensible.
    pub seq: u64,
    pub permission_id: String,
    /// The id of the tool call this request gates, when the source that raised it supplied one.
    /// Both backends do today:
    ///
    /// - `ClaudeSidecarProvider`, since 2026-09-10 (Phase 3) -- its proto `PermissionRequested`
    ///   message carries one directly.
    /// - The legacy Claude-CLI backend, since 2026-09-15, on its `PreToolUse` hook-relay path --
    ///   the real gate. `agent/src/session.rs`'s `permission_requested_event` carries the account
    ///   and the evidence. Its other, secondary source (`can_use_tool`, confirmed leaky) reads the
    ///   id that message's own declared type carries; that message has never been seen on a real
    ///   wire here, so what it produces in practice is unknown rather than known-absent.
    ///
    /// `None` therefore means "this request arrived with no usable link", never "this backend
    /// cannot supply one".
    ///
    /// **`Some("")` is not excluded by anything the type or this crate enforces.** Both in-crate
    /// producers route their ids through `projection::tool_use_link` -- which exists because proto3
    /// has no absent string, so an unset sidecar `tool_use_id` arrives as `""` and would compare
    /// equal to any other record whose id is also `""` -- but `AgentDomainEvent` is public and the
    /// rule is applied at each call site by convention, not by a constructor or a newtype. A third
    /// producer that skips it would land `Some("")` here. Consumers outside this crate (the
    /// frontend) guard the case independently rather than trusting this sentence, and should
    /// keep doing so.
    pub tool_use_id: Option<String>,
    pub tool_name: String,
    pub input: serde_json::Value,
    /// The event's own `provider_prompt`: `Some` when this is the CLI's own prompt rather than the
    /// gate's request. `default` so a record stored before the field existed still loads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_prompt: Option<ProviderPrompt>,
}

/// The CLI's own permission prompt, as the sidecar relays it (Verdandi b3aa188, capability
/// `provider_permission_prompts`; `the private review notes`).
///
/// **Why it exists (O3).** Every v1 session is gated: the CLI runs `default` and a `PreToolUse` hook
/// asks neovibe about every call. The CLI can still ask on its OWN after that hook allowed a call --
/// its sensitive-file safety check on a `Write` under `.git/` or `.claude/` is the measured case --
/// and neither a hook `allow` nor a session rule silences it. With nobody to ask, a headless CLI
/// refused the write that a real `bypassPermissions` session runs. With the capability, that ask
/// arrives as a second `PermissionRequested` for the SAME tool call (same `tool_use_id`, measured
/// but not guaranteed), a new `permission_id`, answered with the same `ResolvePermission`.
///
/// **It is never the permission policy's to answer** (O3 ruling 3): `classify_permission_request`
/// and the saved prefix rules judge the gate's request only. The CLI flagged this call after the gate
/// allowed it, and the CLI itself does not let a rule silence the check. Who answers it is
/// `neovibe_core::agent_backend`'s decision (bypass, or the human's own earlier approval of the same
/// call), never the classifier's.
///
/// Every field is the CLI's own, verbatim, and any may be absent. `reason` is English prose ("Claude
/// requested permissions to edit <path> which is a sensitive file.") -- shown, never parsed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderPrompt {
    /// The SDK's `decisionReason`: why the CLI asked, in its own words.
    pub reason: Option<String>,
    /// The SDK's `description`: the CLI's short subject, e.g. `.git/probe`.
    pub description: Option<String>,
    /// The SDK's `blockedPath`: the path that triggered the prompt.
    pub blocked_path: Option<String>,
    /// Present when one of the user's own `permissions.ask` rules forced this prompt. Such a prompt
    /// is meant for a human, and the SDK's guidance is that a host auto-approving must not approve
    /// it: neovibe draws it as a card in every mode, bypass included (O3 ruling 4).
    pub matched_ask_rule: Option<MatchedAskRule>,
    /// Set, to the raw wire value, when the sidecar named an `origin` this build does not know (a
    /// sidecar newer than this client). Such a prompt is a card in every mode, like one an ask rule
    /// forced (O3 review #3): what a future kind of ask means cannot be judged here, so nothing
    /// automatic answers it. `None` for every origin this build knows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unrecognized_origin: Option<i32>,
}

impl ProviderPrompt {
    /// Whether only a human may answer this prompt, in every mode: the user's own ask rule forced it
    /// (O3 ruling 4), or its kind is unknown to this build (review #3). Bypass, the Auto approval
    /// rule, a bypass entry's `y` and the bypass resync sweep all leave such a prompt a card.
    pub fn needs_a_human(&self) -> bool {
        self.matched_ask_rule.is_some() || self.unrecognized_origin.is_some()
    }

    /// Whose question this is, in the words a card and a row note use: the user's own ask rule when
    /// one forced it; "Claude Code safety check" when the CLI gave its reason (every measured prompt
    /// did: the sensitive-file check); otherwise the neutral "Claude Code asked" (review #5), which is
    /// also what an unknown kind of prompt is called.
    pub fn label(&self) -> String {
        match (&self.matched_ask_rule, self.unrecognized_origin, &self.reason) {
            (Some(rule), _, _) => format!("your ask rule: {}", rule.display()),
            (None, None, Some(_)) => "Claude Code safety check".to_string(),
            _ => "Claude Code asked".to_string(),
        }
    }
}

/// The `permissions.ask` rule behind a provider prompt, verbatim from the SDK's `matchedAskRule`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchedAskRule {
    /// Which settings tier it came from, e.g. `projectSettings` -- which for a project/local session
    /// can be a file in the repository being worked on.
    pub source: String,
    pub tool_name: String,
    /// E.g. `cat:*` for `Bash(cat:*)`; absent for a bare tool-name rule.
    pub rule_content: Option<String>,
}

impl MatchedAskRule {
    /// The rule as Claude Code's own settings spell it: `Bash(cat:*)`, or `Write` for a bare rule.
    pub fn display(&self) -> String {
        match &self.rule_content {
            Some(content) => format!("{}({content})", self.tool_name),
            None => self.tool_name.clone(),
        }
    }
}

/// The one definition of "this provider-supplied id is a real link to a tool call", shared by
/// every producer of a `PermissionRequested` so no layer can invent a second rule.
///
/// Rejects exactly one value: the empty string. That is not defensive padding -- it is what an
/// unset `tool_use_id` genuinely looks like coming out of proto3 on the sidecar path, which has no
/// absent string. `Some("")` handed onward is worse than `None`, because it compares equal to any
/// other record whose id is also `""`, cross-linking two unrelated things. Everything else is
/// passed through byte-for-byte: this function does not own these ids and must not rewrite them,
/// because every link downstream is an equality test against the id the provider also put on the
/// tool call itself.
pub(crate) fn tool_use_link(id: impl Into<String>) -> Option<String> {
    let id = id.into();
    if id.is_empty() {
        None
    } else {
        Some(id)
    }
}

/// The token counts of one usage report, as Verdandi's `TurnUsage` carries them (capability
/// `turn_usage`). The input side is three figures, not one, because the API bills them differently
/// and Claude Code caches aggressively: on a large prompt `input` is typically single digits and
/// nearly all of the prompt is `cache_creation` or `cache_read`, so "did my prompt arrive" is the sum
/// of all three.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
}

/// What a provider actually reported about a session's spend so far. Only ever constructed from
/// figures a provider sent; a provider that sends none produces `None`, never a zeroed `UsageInfo`.
///
/// **A running total, not one turn's spend, on both backends:** the SDK's result carries the session
/// so far ("read the latest result rather than summing across results"), a mid-session `/clear`
/// resets it and a resumed session starts fresh, so a later report replaces an earlier one whole --
/// even a lower one -- and is never added to it.
///
/// Each backend fills the half it really has and leaves the rest `None`, which is unknown, not zero:
/// the sidecar carries tokens and a model but no turn count (`TurnUsage` has none); the legacy
/// backend carries a turn count but no tokens or model (its `ResultLine` reads no token fields).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageInfo {
    pub total_cost_usd: f64,
    /// Legacy's `result.num_turns`; `None` from the sidecar, whose `TurnUsage` has none.
    pub num_turns: Option<u32>,
    /// `None` from legacy, whose `ResultLine` reads no token fields.
    pub tokens: Option<TokenUsage>,
    /// `TurnUsage.model`: the model with the most tokens; `None` when empty.
    pub model: Option<String>,
}

/// Neovibe's sole product-state authority for one conversation (design doc §10.3). `pending_permissions`
/// is a map (not the old `Vec`) so a caller can look up/remove one specific request by id in O(1)
/// -- exactly what answering one permission card needs, and exactly what the design doc's own
/// "pending permissions map" wording asks for.
///
/// **This type is `Serialize` and deliberately not `Deserialize`, and the six record types above
/// are both.** A projection that can be read off disk is a projection that can arrive carrying
/// `status: Running`, an `active_turn_id` for a turn nobody is running, and two session ids for
/// sessions that ended -- i.e. a declaration of live state restored from a file. Restoring history
/// (`history::store`) is refolding data through `apply`, never rehydrating a state declaration, and
/// leaving this derive off is what makes the difference a compiler error rather than a discipline.
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
    /// Prompts the user sent, in order, each carrying the `seq` that orders it against the other
    /// three collections.
    pub user_prompts: Vec<UserPromptRecord>,
    /// Assistant messages, in arrival order, each carrying the `seq` that orders it against
    /// `user_prompts`, `tool_calls` and `pending_permissions`.
    ///
    /// Design doc §10.3 calls for "ordered transcript items" as a single interleaved sequence with
    /// tool calls. The four collections are still stored apart -- a consumer that only wants tool
    /// calls should not have to filter a sum type for them -- but they are no longer unorderable:
    /// `seq` is a total order over all four, so a reader can interleave them exactly. That is the
    /// part the frontend genuinely could not do for itself, because a snapshot replaces its whole
    /// state and carried no arrival order at all.
    pub transcript: Vec<TranscriptMessage>,
    pub tool_calls: Vec<ToolCallRecord>,
    pub pending_permissions: HashMap<String, PermissionRequestRecord>,
    /// The last usage a provider actually reported, or `None` if none ever has -- the state of a
    /// session before its first report, of one resumed from history (which seeds none), and of a
    /// sidecar that predates `TurnUsage`. A consumer must render `None` as unknown; rendering it as
    /// zero re-tells the exact lie this field was made an `Option` to stop. Each report replaces it
    /// whole, even with a lower figure: the SDK reports a running total and `/clear` resets it.
    pub usage: Option<UsageInfo>,
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
    /// Set once, before any live event is folded, when a resumed conversation was seeded with what
    /// it already said (`history::load`). `None` on every fresh session and on a resume that found
    /// nothing to restore.
    ///
    /// **Not an item and not ordered.** It occupies no `seq`, is never produced by `apply`, and is
    /// not part of the four collections -- it is one statement ABOUT them: how many of them came
    /// off a disk, which disk, and where the live part starts (`upto_seq`). `apply` leaves it
    /// exactly as it found it, which is what makes "the history notice describes the seed" true for
    /// the whole life of the projection rather than only at the moment of loading.
    pub history: Option<HistoryNotice>,
    /// The first `UngatedCliMode` this session reported (spec §2.3, D12), or `None`.
    ///
    /// Kept rather than folded away because the event alone does not survive a
    /// `UiDelivery::Resync`: an overflowing UI queue drops its events and the host rebuilds from
    /// this projection, and a tripwire that vanished there would leave an ungated session running
    /// until its next report. `neovibe_core`'s pump reads this after every delivery. It changes
    /// nothing a view draws -- the status is untouched and it is not serialized; closing the session
    /// is the host's to do.
    #[serde(skip)]
    pub ungated_cli_mode: Option<UngatedCliModeRecord>,
}

/// Which of the two records a restored history was read from.
///
/// The distinction is user-facing on purpose (§7.2, §8): the two can genuinely differ -- Claude's
/// own transcript is what the CLI maintains and what a `claude --resume` in a terminal shows, while
/// Neovibe's copy is this side's unilateral record. A user with both open must be able to see at a
/// glance which one the panel is showing, so this is never silently degraded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HistorySource {
    /// B: the real Claude CLI's own `<uuid>.jsonl`.
    ClaudeTranscript,
    /// A: `history::store`, Neovibe's own copy, used only when B could not be read.
    NeovibeCopy,
}

/// Everything the panel needs to say one honest line about a restored history.
///
/// Design §5.5. Every field here answers a question a user can actually ask -- which conversation
/// is this, where did it come from, is anything missing, and why am I looking at the fallback --
/// and nothing here is decoration: this struct is the sole input to the notice row, which is
/// rendered whenever history was restored at all, not only when it was truncated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HistoryNotice {
    pub source: HistorySource,
    /// Conversational items actually folded in: prompts, assistant messages and tool calls. The
    /// TRUE count, which is usually below `HISTORY_MAX_ITEMS` even when truncation happened,
    /// because the character budget binds first about as often.
    pub restored_items: usize,
    /// Items dropped by TRUNCATION. `None` means records were certainly omitted but cannot be
    /// counted -- the scan started partway into a file too large to read whole. Never counts a
    /// skipped line type (attachments, side records, unknown lines): those are not omitted
    /// conversation and go to the log instead.
    pub omitted_items: Option<usize>,
    /// The `seq` bound (exclusive) of the restored part: every historical item sits below it and
    /// every live item at or above it. Present whenever history was restored, not only when it was
    /// truncated -- the panel needs it to tell a historical tool call with no result (which can
    /// never complete) from a live one that is still running (§3.3).
    pub upto_seq: u64,
    /// The file this history was actually read from. Always non-empty: a notice exists only when
    /// history was restored, and history can only come from one file.
    pub source_path: String,
    /// The Claude transcript that was looked for and not used. `None` when `source` is
    /// `ClaudeTranscript` (it would repeat `source_path`) and when no path could be built at all.
    pub attempted_transcript_path: Option<String>,
    /// Why B was not used, in the user's own terms. `Some` exactly when `source` is `NeovibeCopy`.
    pub fallback_reason: Option<String>,
    /// The CLI version that wrote the transcript, from the file itself. B only -- A has no such
    /// concept. **Not a schema version**; nothing branches on it.
    pub writer_version: Option<String>,
}

impl AgentSessionProjection {
    /// The reducer: folds one event's effect into this projection, unconditionally bumping
    /// `last_revision` by exactly one regardless of whether the event has any other observable
    /// effect (an event with a real revision but no other effect -- e.g. a `ContentDelta` with
    /// `ContentKind::Thinking` -- is still a real, ordered occurrence a WebView bridge must be
    /// able to skip past correctly when replaying from a revision, not something invisible to the
    /// counter). Pure and side-effect-free -- no I/O -- so it's trivially testable without
    /// spawning anything.
    ///
    /// Ordering: an item created by this call takes `seq = self.last_revision` as read on entry --
    /// the value BEFORE the bump at the bottom. Two consequences the rest of the system relies on.
    /// (1) No two items ever share a `seq`, because no single event creates more than one item and
    /// every call bumps the counter exactly once, so `seq` is a total order over all four
    /// collections. (2) Every `seq` is strictly less than `last_revision` afterwards, so a
    /// snapshot's `throughRevision` is a strict upper bound on the seqs inside it -- which is what
    /// lets the frontend seed its own counter from that number and never collide with an item the
    /// snapshot already carried.
    ///
    /// Neither half is left as prose: `agent/tests/projection.rs` pins (1) in
    /// `no_single_event_ever_creates_more_than_one_item` and (2) in
    /// `every_seq_is_strictly_below_the_revision_a_snapshot_would_report`. (1) needs a test more
    /// than (2) does, because breaking it fails quietly -- a future arm pushing two items would
    /// give both the same number, and every consumer that sorts on `seq` (`buildTimeline`, and the
    /// snapshot merge in `core/src/agent_bridge.rs`'s own tests) would then tie them in whatever
    /// order the arrays happened to be in, which is a mis-ordered pair rather than a failure.
    pub fn apply(&mut self, event: &AgentDomainEvent) {
        let seq = self.last_revision;
        match event {
            AgentDomainEvent::SessionOpened {
                session_id,
                provider_session_id,
                model,
                cwd,
            } => {
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
            AgentDomainEvent::UserPromptSubmitted { text } => {
                self.user_prompts.push(UserPromptRecord {
                    seq,
                    text: text.clone(),
                });
                // Anything that can only occur BETWEEN assistant messages closes the run. A prompt
                // is the clearest such thing: without this, the reply to the next turn is appended
                // to the reply to the last one and they render as a single message.
                self.assistant_message_open = false;
            }
            AgentDomainEvent::ContentDelta {
                kind: ContentKind::Text,
                text,
                ..
            } => {
                match self.transcript.last_mut() {
                    // Appending leaves `seq` alone: a message is ordered by where it started.
                    Some(open) if self.assistant_message_open => open.text.push_str(text),
                    _ => {
                        self.transcript.push(TranscriptMessage {
                            seq,
                            text: text.clone(),
                        });
                        self.assistant_message_open = true;
                    }
                }
            }
            AgentDomainEvent::ContentDelta {
                kind: ContentKind::Thinking,
                ..
            } => {
                // No projection effect -- mirrors the pre-Phase-1 AgentSessionState::apply's
                // identical treatment of `Thinking`. Still bumps last_revision (see fn doc).
            }
            AgentDomainEvent::AssistantMessageBoundary { .. } => {
                self.assistant_message_open = false;
            }
            AgentDomainEvent::ToolCallStarted {
                turn_id,
                tool_use_id,
                name,
                input,
            } => {
                // A tool call can only happen between assistant messages, so whatever text was
                // streaming has ended; the text after it is a new message.
                self.assistant_message_open = false;
                self.tool_calls.push(ToolCallRecord {
                    seq,
                    turn_id: turn_id.clone(),
                    tool_use_id: tool_use_id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                    result: None,
                });
            }
            AgentDomainEvent::ToolCallCompleted {
                tool_use_id,
                content,
                is_error,
                ..
            } => {
                if let Some(call) = self.tool_calls.iter_mut().find(|c| &c.tool_use_id == tool_use_id) {
                    call.result = Some(ToolCallResult {
                        content: content.clone(),
                        is_error: *is_error,
                    });
                }
            }
            AgentDomainEvent::PermissionRequested {
                permission_id,
                tool_use_id,
                tool_name,
                input,
                provider_prompt,
            } => {
                self.assistant_message_open = false;
                // Keyed by `permission_id`, never by `tool_use_id`: a provider prompt is a SECOND
                // request for a call whose gate request was already answered, and it must land as a
                // card of its own rather than be taken for a duplicate (O3 ruling 7).
                self.pending_permissions.insert(
                    permission_id.clone(),
                    PermissionRequestRecord {
                        seq,
                        permission_id: permission_id.clone(),
                        tool_use_id: tool_use_id.clone(),
                        tool_name: tool_name.clone(),
                        input: input.clone(),
                        provider_prompt: provider_prompt.clone(),
                    },
                );
            }
            AgentDomainEvent::PermissionResolved { permission_id, .. } => {
                self.pending_permissions.remove(permission_id);
            }
            AgentDomainEvent::TurnCompleted { usage, .. } => {
                self.active_turn_id = None;
                self.assistant_message_open = false;
                // Conditional, where this arm used to assign unconditionally -- which is how the
                // sidecar path's fabricated zero reached the projection in the first place. A turn
                // that reported no usage leaves whatever was last reported standing: silence is not
                // a measurement, so it must not overwrite one. A turn that DID report replaces the
                // figure whole and never adds to it or keeps the larger: the SDK reports a running
                // total that `/clear` resets, so a smaller later figure is the truth.
                if let Some(reported) = usage {
                    self.usage = Some(reported.clone());
                }
            }
            // Both endings clear `active_turn_id`, and for one reason: no `TurnCompleted` is ever
            // coming. Leaving it set is what turned a dead session into a spinner that never stops
            // -- `App.tsx` derives `turnInProgress` from exactly this field, and
            // `supervisor_client::derive_status` reports `Working` from it ahead of any status
            // check, so a killed provider showed up as a busy agent in the dashboard too. This is
            // not a synthesized `TurnCompleted`: no completion is recorded, no outcome is invented,
            // and no result text appears. The turn simply stops being in progress, because it is
            // not.
            // A resume verdict is a provider-reported fact, and the two bad ones end the session's
            // usefulness whatever else follows. Turning them into a status here is not synthesizing
            // anything: the provider said it, and a conversation that did not continue what the user
            // asked for must not look like one that did.
            AgentDomainEvent::ResumeOutcome {
                requested_provider_session_id,
                status,
                attached_provider_session_id,
                forked,
                detail,
            } => {
                if !status.attached_to_the_requested_session(
                    requested_provider_session_id,
                    attached_provider_session_id.as_deref(),
                    *forked,
                ) {
                    self.active_turn_id = None;
                    self.assistant_message_open = false;
                    self.status = ProjectionStatus::Unavailable {
                        reason: describe_failed_resume(
                            requested_provider_session_id,
                            *status,
                            attached_provider_session_id.as_deref(),
                            detail.as_deref(),
                        ),
                    };
                }
            }
            AgentDomainEvent::SessionUnavailable { reason } => {
                self.active_turn_id = None;
                self.assistant_message_open = false;
                self.status = ProjectionStatus::Unavailable { reason: reason.clone() };
            }
            AgentDomainEvent::SessionClosed { reason } => {
                self.active_turn_id = None;
                self.assistant_message_open = false;
                self.status = ProjectionStatus::Closed { reason: reason.clone() };
            }
            // The host's answer mode is the tab's (`Tab::mode`), and nothing a provider reports moves
            // it (spec §2.2); a report of `default` is therefore nothing to fold.
            AgentDomainEvent::PermissionModeChanged { .. } => {}
            // Recorded, first report wins, and nothing else changes: the host closes the session
            // (`TabSet::pump`), and until it has, the projection says what it was told.
            AgentDomainEvent::UngatedCliMode { reported, detail } => {
                if self.ungated_cli_mode.is_none() {
                    self.ungated_cli_mode = Some(UngatedCliModeRecord {
                        reported: reported.clone(),
                        detail: detail.clone(),
                    });
                }
            }
        }
        self.last_revision += 1;
    }
}

/// The user-facing reason a resume did not continue the session that was asked for.
///
/// Says which conversation was wanted, because a user who has several workspaces has no other way
/// to tell which one just failed, and always ends with what to do instead -- an error that only
/// reports a fault leaves the reader stuck.
pub fn describe_failed_resume(
    requested: &str,
    status: ResumeStatus,
    attached: Option<&str>,
    detail: Option<&str>,
) -> String {
    let suffix = match detail {
        Some(detail) if !detail.trim().is_empty() => format!(" ({detail})"),
        _ => String::new(),
    };
    match status {
        ResumeStatus::Rejected => format!(
            "the provider does not have session {requested} any more, so that conversation cannot be \
             continued{suffix}. Start a new session instead."
        ),
        ResumeStatus::InitializationFailed => format!(
            "the provider failed to start while continuing session {requested}{suffix}. This is a \
             provider problem rather than a missing conversation -- the session may still exist. \
             Try again, or start a new session."
        ),
        // Reached only when the ids disagree and this was not a fork: the provider opened a real
        // session, just not the one that was asked for. Treated as a failure rather than accepted,
        // because a conversation that looks continued while carrying none of its history is the
        // single outcome the whole resume protocol exists to prevent.
        ResumeStatus::Attached => format!(
            "asked to continue session {requested}, but the provider attached to {} instead, so this \
             conversation carries none of the history that was asked for{suffix}. Start a new session \
             instead.",
            attached.unwrap_or("an unnamed session"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The empty-string case is not hypothetical. On the sidecar path `tool_use_id` crosses
    /// proto3, which has no absent string: a field the provider never set arrives as `""`, not as
    /// a missing one. `Some("")` would then travel all the way to the frontend as a link, where
    /// it would match any OTHER record whose id is also `""` -- cross-linking two unrelated
    /// things. The frontend already refuses `""` on both sides of that link; this makes the
    /// domain layer refuse to hand it one in the first place.
    #[test]
    fn an_empty_tool_use_id_is_not_a_link() {
        assert_eq!(tool_use_link(""), None);
    }

    #[test]
    fn a_real_tool_use_id_is_a_link_and_is_not_rewritten() {
        assert_eq!(
            tool_use_link("toolu_01CtdezhmhUCrBaswxW5HYmC"),
            Some("toolu_01CtdezhmhUCrBaswxW5HYmC".to_string())
        );
    }

    /// Whitespace is deliberately NOT trimmed or treated as empty: this function's job is to
    /// reject the one value that is known to mean "unset" on a real wire, not to sanitize ids it
    /// does not own. An id that genuinely contained a space would still be the provider's own
    /// identifier, and silently rewriting it would break the equality every link depends on.
    #[test]
    fn a_whitespace_id_is_left_exactly_as_the_provider_sent_it() {
        assert_eq!(tool_use_link(" "), Some(" ".to_string()));
    }
}
