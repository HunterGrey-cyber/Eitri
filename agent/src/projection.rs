//! The v2 reducer stage sitting above `agent::event::AgentEvent` (Claude-wire-specific, stays
//! crate-internal beyond `agent::session`'s own translation step): folds a stream of
//! provider-neutral `AgentDomainEvent`s into a queryable `AgentSessionProjection`, this crate's
//! sole product-state authority
//!.
//!
//! Deliberately NOT modeled here, per that same design doc and this crate's own YAGNI discipline
//! (no field with zero current consumer): `conversation_id`/`provider`/`provider_session_id` as
//! separate identities (this phase has exactly one provider and no resumable session-id split --
//! `session_id` alone is what today's single Claude CLI process actually has), `capabilities`,
//! provider health/compatibility warning, and handoff state (all Phase 4+ concerns). Revisit this
//! struct's shape when whichever later phase actually needs one of those, not before.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentKind {
    Text,
    Thinking,
}

/// A turn's terminal state distinguishes completion, interruption, failure and a reached limit.
/// `LimitReached` comes only from the sidecar, which maps the SDK's `terminal_reason`
/// (`max_turns`, `blocking_limit`, `rapid_refill_breaker`, `budget_exhausted`) to it.
/// The legacy backend's translation never chooses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnOutcome {
    Completed,
    Interrupted,
    Failed,
    LimitReached,
}

/// What the provider said about a turn that did not simply complete, carried on
/// [`AgentDomainEvent::TurnCompleted`] and kept on the [`TurnEndingRecord`] drawn where it ended.
/// Every field is `None` when the provider said nothing: a completed turn, an end the sidecar
/// synthesized after its CLI died, and most legacy results.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnEndDetail {
    /// The provider's terminal reason as it gave it (`max_turns`, `blocking_limit`, `api_error`,
    /// ...). An open set: the panel words the ones it knows and shows the rest verbatim.
    pub reason: Option<String>,
    /// The HTTP status of the API error that ended the turn, when the provider reported one.
    pub api_error_status: Option<u32>,
    /// The provider's own words about the failure, trimmed and at most
    /// [`MAX_TURN_END_MESSAGE_CHARS`] characters (cut with `…`). Never set for a completed or an
    /// interrupted turn: an interrupt is the user's own doing, and the CLI's diagnostics for one
    /// (`[ede_diagnostic] ...`) are noise to them.
    pub message: Option<String>,
}

/// The longest provider message a turn's ending keeps. The row is meant to be one short line plus
/// what the provider said; the SDK's own error list allows 8 entries of 2000 characters each,
/// which would bury the conversation under it.
pub const MAX_TURN_END_MESSAGE_CHARS: usize = 400;

impl TurnEndDetail {
    /// Builds the detail from what a result carried. The message is the provider's error list
    /// when it gave one (an error result has no result text), else the result text of a result
    /// that says it is an error (an API error arrives as a "success" result whose text is the
    /// error). A result that is not an error -- a turn a hook stopped, say -- has the reply as its
    /// text, which is not a reason and is not repeated.
    pub fn from_result(
        outcome: TurnOutcome,
        is_error: bool,
        result_text: &str,
        reason: Option<String>,
        api_error_status: Option<u32>,
        errors: &[String],
    ) -> Self {
        let message = match outcome {
            TurnOutcome::Completed | TurnOutcome::Interrupted => None,
            TurnOutcome::Failed | TurnOutcome::LimitReached => {
                let listed = errors
                    .iter()
                    .map(|e| e.trim())
                    .filter(|e| !e.is_empty())
                    .collect::<Vec<_>>()
                    .join("\n");
                let text = match (listed.is_empty(), is_error) {
                    (false, _) => listed.as_str(),
                    (true, true) => result_text.trim(),
                    (true, false) => "",
                };
                (!text.is_empty()).then(|| cap_chars(text, MAX_TURN_END_MESSAGE_CHARS))
            }
        };
        Self {
            reason: reason.map(|r| r.trim().to_string()).filter(|r| !r.is_empty()),
            api_error_status,
            message,
        }
    }
}

fn cap_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        None => text.to_string(),
        Some((cut, _)) => format!("{}…", text[..cut].trim_end()),
    }
}

/// How a turn ended when it did not simply complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnEndingKind {
    Interrupted,
    Failed,
    LimitReached,
    /// The session ended (lost, closed, or a resume that did not continue) while the turn was
    /// still running, and no `TurnCompleted` came for it.
    Lost,
}

/// A turn that ended other than by completing, as an item of the transcript at the place it
/// ended. Without it a failed, interrupted or limit-stopped turn read exactly like a finished one:
/// the activity line went away and nothing said why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnEndingRecord {
    /// Ordering key, shared with the other collections. See `AgentSessionProjection::apply`.
    pub seq: u64,
    pub turn_id: String,
    pub kind: TurnEndingKind,
    pub detail: TurnEndDetail,
    /// When the account's usage window reset as of this ending: the latest limit report's reset time
    /// when the turn ended, or, when none was known then, that of the first report before the next turn
    /// starts (an agent may report the limit just after the turn it stopped). Kept with the ending, so
    /// a later report never changes what an older row says.
    #[serde(default)]
    pub resets_at: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOutcome {
    Allowed,
    Denied,
    CancelledByInterrupt,
    CancelledBySessionClose,
    /// The provider process itself failed/crashed while this permission was pending -- the sidecar
    /// proto's `PERMISSION_OUTCOME_PROVIDER_FAILED`. No legacy-backend producer; only
    /// `ClaudeSidecarProvider`'s translation can produce this.
    ProviderFailed,
    /// A permission request timed out waiting for a decision -- the sidecar proto's
    /// `PERMISSION_OUTCOME_EXPIRED`. No legacy-backend producer either.
    Expired,
    /// This host answered `PermissionDecision::Defer` and the CLI's own permission handling decided
    /// the call -- the sidecar proto's `PERMISSION_OUTCOME_DEFERRED`. Says nothing about whether the
    /// call ran: a refusal arrives as `AgentDomainEvent::PermissionDenied` and as the call's error
    /// result. No legacy-backend producer.
    Deferred,
}

/// What the provider said about a resume. Mirrors the wire's `ResumeStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// (`panel/src/inbound.rs`); rendering that would show a file path and a selection nobody
    /// wrote.
    UserPromptSubmitted {
        text: String,
    },
    /// A message the user put into the running turn (`AgentProvider::steer_turn`) has been read by the
    /// agent: the provider's own report, never assumed by the host. Folded as a user prompt at this
    /// point of the conversation, which is where the agent took it in -- not where Enter was pressed.
    /// `text` is what the user typed, as for `UserPromptSubmitted`.
    UserMessageDelivered {
        client_message_id: String,
        turn_id: String,
        text: String,
    },
    /// A message put into the running turn did not reach the agent: it refused it, or the turn ended
    /// before reading it. Adds nothing to the conversation; the host holds the message again and says
    /// why (`reason`, in Eitri's words with the agent's own after a colon when it gave any).
    UserMessageDropped {
        client_message_id: String,
        reason: String,
    },
    ContentDelta {
        turn_id: String,
        kind: ContentKind,
        text: String,
    },
    /// Ends the streaming assistant message so the next text starts a new transcript entry,
    /// even when no tool call separates the messages. `ContentDelta` carries no message identity,
    /// so consecutive messages would otherwise merge. The legacy backend emits this when a
    /// text block's `message.id` changes. The sidecar emits it from `TextDelta.message_id` when
    /// it advertises `text_delta_message_id`; without that capability it emits no boundary.
    /// See `providers::claude_sidecar::translate::MessageSplit`.
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
        /// `Some` when the CLI itself asked after the `PreToolUse` gate had already answered:
        /// `PERMISSION_ORIGIN_PROVIDER_PROMPT`, with capability `provider_permission_prompts`.
        /// `None` for the gate's request: every legacy request and sidecar origins HOOK or UNSPECIFIED.
        /// See [`ProviderPrompt`]. Omitted from the serialized event when `None` so gate requests
        /// keep their existing serialized shape.
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
        /// Why the turn ended, in the provider's terms, when it did not simply complete.
        detail: TurnEndDetail,
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
    /// Represents a sidecar `PermissionModeChanged` acknowledgment of `SetPermissionMode`
    /// with capability `set_permission_mode`. This client issues no such RPC, but still decodes
    /// the event because the pinned protocol can emit it. A `default` report is folded as a
    /// no-op; a less restrictive report becomes `UngatedCliMode` instead.
    PermissionModeChanged {
        mode: crate::PermissionMode,
        /// `SetPermissionModeResponse.permission_mode` verbatim -- `default` / `bypassPermissions`,
        /// the provider's OWN vocabulary, kept for diagnostics rather than collapsed into `mode`.
        provider_mode: String,
        /// True when entering bypass applied Verdandi's conservative floor
        /// (`PermissionModeChanged.bypass_default_deny_applied`). Eitri states `unrestricted` on
        /// every session (`build_create_request`), so this must never happen in practice; see the
        /// translate-side match arm for what a `true` here means.
        floor_applied: bool,
    },
    /// The CLI reported a mode that `crate::classify_cli_mode` classifies as `Ungated` rather than
    /// at least as restrictive as the requested `default`. An unanswered hook in that mode can let
    /// the tool run, so the session must close.
    ///
    /// The sidecar emits this for `SessionReady.permission_mode` or `PermissionModeChanged`.
    /// Legacy emits it for a `PreToolUse` payload after denying the call. The host's
    /// `tab_set::TabSet::pump` closes the session and fails the tab.
    UngatedCliMode {
        /// The mode as the CLI named it, verbatim (`bypassPermissions`, `acceptEdits`, ...).
        reported: String,
        /// Which report it came from: `SessionReady`, `PermissionModeChanged`, or `a PreToolUse hook
        /// call`.
        detail: String,
    },
    /// What the CLI said its permission mode is, on a session that asked for the CLI's own `auto`
    /// under the gate (`ProviderCapabilities::cli_auto_mode`), each time it says it and it is a mode
    /// that session accepts: `auto`, or `default` when the CLI could not run auto. Anything else
    /// becomes `UngatedCliMode` instead. Never produced by a session that asked for `default`, so
    /// every such session's events are what they were before this existed.
    ///
    /// Read by the host's answer path (`eitri_core::agent_backend`), which defers a request to the
    /// CLI's classifier only while the latest report is `auto`: a CLI that fell back to `default`
    /// gets Eitri's own policy, as a session that never asked for auto does.
    CliPermissionMode {
        /// The CLI's own word, verbatim.
        reported: String,
    },
    /// The CLI refused a tool call on its own, before asking anyone (Verdandi `PermissionDenied`,
    /// capability `permission_denied_events`): its auto-mode classifier, a deny rule in a loaded
    /// settings tier, or its mode. The model already has the refusal as the call's error result;
    /// this is what lets the panel say who refused it. Only a session that stated a CLI permission
    /// mode receives these, and Eitri states one only when it asks for auto.
    PermissionDenied {
        /// The call it refused, the same id its `ToolCallStarted` carried; `None` when the CLI gave
        /// none, which leaves nothing to attach it to.
        tool_use_id: Option<String>,
        tool_name: String,
        /// What decided, in the CLI's own word (`classifier`, `rule`, `mode`, ...), when it said.
        reason_type: Option<String>,
        /// The deciding component's own words (`[Git Destructive]`), English, shown and never
        /// parsed, when it gave any.
        reason: Option<String>,
    },
    /// An agent that is not the Claude sidecar reported a sandbox, approval policy or reviewer other
    /// than the one Eitri asked for. The host fails the tab and stops answering requests on its own,
    /// bypass included (`eitri_core::tab_set::TabSet::pump`).
    PolicyMismatch {
        /// What Eitri sent, in the agent's own words.
        requested: String,
        /// What the agent reported, in the agent's own words.
        reported: String,
    },
    /// How many calls the agent left running after a turn ended (a command it put in the background),
    /// whenever that number changes. Sent before the `TurnCompleted` of the turn that left them, so the
    /// turn's review knows that later changes are not in it. Calls a running turn starts are not counted
    /// until it ends.
    BackgroundCalls {
        running: u32,
    },
    /// The slash commands the agent says this session accepts, whole, each time it says so (ACP
    /// `available_commands_update`). The latest list replaces the earlier one; an empty list withdraws
    /// them. Only reported by an agent that has such a list: a session whose agent never reports one
    /// keeps `available_commands` at `None`, and the panel falls back to what it knows of that agent.
    CommandsAvailable {
        commands: Vec<SlashCommand>,
    },
    /// The account's usage limit changed, as the agent reported it. Account-wide, not per turn.
    AccountLimits {
        /// How much of the window is used, in percent.
        used_percent: f64,
        /// Length of the window, when the agent said.
        window_minutes: Option<u32>,
        /// When the window resets (Unix seconds), when the agent said.
        resets_at: Option<i64>,
        /// The plan the limit belongs to, when the agent said.
        plan: Option<String>,
    },
    /// The CLI's own prompt for a call carried another input than the gate was asked about for the
    /// same `tool_use_id` (something after the gate, such as a loaded settings tier's `PreToolUse`
    /// hook returning `updatedInput`, changed it). **Never read off the provider's wire:** the
    /// projection derives it ([`AgentSessionProjection::derived_after`]) when ingestion folds the
    /// prompt, and ingestion folds and delivers it right after that prompt, so the panel learns of
    /// the change even when the host answered the gate request (and so never delivered it). It is
    /// the one place the comparison is made; the panel only applies it.
    ToolInputChanged {
        tool_use_id: String,
        change: InputChange,
    },
    /// A conversation's settled records, carried from the window that held it into this one, with
    /// their own sequence numbers and the revision they were taken at. **Made only by the move**
    /// ([`AgentSessionProjection::adopt`]), never by a provider: it is how an adopted projection is
    /// still built by folding alone. Taken only by a projection that holds no record and was never
    /// seeded; anywhere else it changes nothing but the revision.
    MoveSeeded(Box<crate::move_seed::MoveSeed>),
}

/// The longest string in a [`InputChange`], in characters; longer ones are cut with `…`.
pub const MAX_CHANGED_VALUE_CHARS: usize = 1000;
/// The most a [`InputChange`]'s `fields` hold, serialized; a field that would pass it is left out.
pub const MAX_CHANGED_FIELDS_BYTES: usize = 4000;
/// The most keys (and key characters) an input is remembered or reported by.
const MAX_INPUT_KEYS: usize = 64;
const MAX_KEY_CHARS: usize = 200;
/// The key a non-object input is compared and reported under.
const WHOLE_INPUT_KEY: &str = "(input)";

/// How a call's input, as the CLI's own prompt carried it, differs from the gate's: only the
/// top-level fields that differ, with the CLI's values, bounded ([`MAX_CHANGED_VALUE_CHARS`],
/// [`MAX_CHANGED_FIELDS_BYTES`]). A rewritten `Write` therefore never copies its body twice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputChange {
    /// The CLI's value of every field that is new or differs from the gate's.
    pub fields: serde_json::Map<String, serde_json::Value>,
    /// Fields the gate saw that the CLI's input no longer has.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<String>,
    /// Something above was cut or left out to stay inside the bounds.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clipped: bool,
}

/// What the projection remembers of a gate request's input: a digest per top-level field, not the
/// input, so held inputs stay small whatever a `Write` carried.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GateInput {
    /// Whether the input was a JSON object; an input of any other kind is one field under `""`.
    object: bool,
    #[serde(with = "held_fields")]
    fields: std::collections::BTreeMap<FieldId, HeldField>,
}

/// The held fields as a list of pairs: a JSON object's keys are strings, and a field's key is not.
mod held_fields {
    use super::{FieldId, HeldField};
    use serde::{Deserialize, Deserializer, Serializer};
    use std::collections::BTreeMap;

    pub(super) fn serialize<S: Serializer>(fields: &BTreeMap<FieldId, HeldField>, out: S) -> Result<S::Ok, S::Error> {
        out.collect_seq(fields.iter())
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(input: D) -> Result<BTreeMap<FieldId, HeldField>, D::Error> {
        let pairs: Vec<(FieldId, HeldField)> = Vec::deserialize(input)?;
        Ok(pairs.into_iter().collect())
    }
}

/// How a field is keyed in the gate memory. A name too long to hold is keyed by the digest of its
/// bytes in a variant of its own, so no literal name, whatever it spells, can be taken for it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
enum FieldId {
    Literal(String),
    Hashed([u8; 32]),
}

/// One remembered field: the digest of its value and, for a long name, the cut name to report it by.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct HeldField {
    digest: [u8; 32],
    /// `Some` only for a [`FieldId::Hashed`] key: the name cut to [`MAX_KEY_CHARS`] with `…`.
    cut_name: Option<String>,
}

fn top_level(input: &serde_json::Value) -> Vec<(&str, &serde_json::Value)> {
    match input {
        serde_json::Value::Object(map) => map.iter().map(|(k, v)| (k.as_str(), v)).collect(),
        other => vec![("", other)],
    }
}

/// A field name as the gate memory keys it: the name itself, or for one longer than
/// [`MAX_KEY_CHARS`] a digest of it, so what is held stays small whatever a key looks like.
fn key_id(key: &str) -> FieldId {
    use sha2::{Digest, Sha256};
    if key.chars().nth(MAX_KEY_CHARS).is_none() {
        return FieldId::Literal(key.to_string());
    }
    FieldId::Hashed(Sha256::digest(key.as_bytes()).into())
}

/// A digest that ignores object key order and tells `1` from `1.0`, as `Value ==` does.
fn digest(value: &serde_json::Value) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    fn feed(hasher: &mut Sha256, value: &serde_json::Value) {
        use serde_json::Value;
        match value {
            Value::Null => hasher.update([0]),
            Value::Bool(b) => hasher.update([1, u8::from(*b)]),
            Value::Number(n) => {
                let text = n.to_string();
                hasher.update([2]);
                hasher.update((text.len() as u64).to_le_bytes());
                hasher.update(text.as_bytes());
            }
            Value::String(text) => {
                hasher.update([3]);
                hasher.update((text.len() as u64).to_le_bytes());
                hasher.update(text.as_bytes());
            }
            Value::Array(items) => {
                hasher.update([4]);
                hasher.update((items.len() as u64).to_le_bytes());
                for item in items {
                    feed(hasher, item);
                }
            }
            Value::Object(map) => {
                hasher.update([5]);
                hasher.update((map.len() as u64).to_le_bytes());
                let mut entries: Vec<_> = map.iter().collect();
                entries.sort_by(|a, b| a.0.cmp(b.0));
                for (key, item) in entries {
                    hasher.update((key.len() as u64).to_le_bytes());
                    hasher.update(key.as_bytes());
                    feed(hasher, item);
                }
            }
        }
    }
    let mut hasher = Sha256::new();
    feed(&mut hasher, value);
    hasher.finalize().into()
}

impl GateInput {
    /// `None` for an input with more fields than [`MAX_INPUT_KEYS`]: nothing is remembered, so
    /// nothing is compared, and no comparison is never a mark.
    fn of(input: &serde_json::Value) -> Option<Self> {
        let fields = top_level(input);
        (fields.len() <= MAX_INPUT_KEYS).then(|| GateInput {
            object: input.is_object(),
            fields: fields
                .into_iter()
                .map(|(k, v)| {
                    let id = key_id(k);
                    let cut_name = matches!(id, FieldId::Hashed(_)).then(|| clip_chars(k, MAX_KEY_CHARS).0);
                    (
                        id,
                        HeldField {
                            digest: digest(v),
                            cut_name,
                        },
                    )
                })
                .collect(),
        })
    }
}

fn clip_chars(text: &str, max: usize) -> (String, bool) {
    match text.char_indices().nth(max) {
        Some((at, _)) => (format!("{}…", &text[..at]), true),
        None => (text.to_string(), false),
    }
}

/// `value` with its strings and keys cut. Copies the whole value, so it is only for one already
/// known to fit the byte budget ([`clip_text`] says so without walking more than the budget).
fn clip_value(value: &serde_json::Value, clipped: &mut bool) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::String(text) => {
            let (text, cut) = clip_chars(text, MAX_CHANGED_VALUE_CHARS);
            *clipped |= cut;
            Value::String(text)
        }
        Value::Array(items) => Value::Array(items.iter().map(|v| clip_value(v, clipped)).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| {
                    let (key, cut) = clip_chars(k, MAX_KEY_CHARS);
                    *clipped |= cut;
                    (key, clip_value(v, clipped))
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

/// A [`serde_json::Value`] serialized with its strings and keys cut as [`clip_value`] cuts them,
/// without building the cut copy.
struct Clipped<'a>(&'a serde_json::Value);

impl Serialize for Clipped<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{SerializeMap, SerializeSeq};
        use serde_json::Value;
        match self.0 {
            Value::String(text) => serializer.serialize_str(&clip_chars(text, MAX_CHANGED_VALUE_CHARS).0),
            Value::Array(items) => {
                let mut seq = serializer.serialize_seq(Some(items.len()))?;
                for item in items {
                    seq.serialize_element(&Clipped(item))?;
                }
                seq.end()
            }
            Value::Object(map) => {
                let mut out = serializer.serialize_map(Some(map.len()))?;
                for (key, item) in map {
                    out.serialize_entry(&clip_chars(key, MAX_KEY_CHARS).0, &Clipped(item))?;
                }
                out.end()
            }
            other => other.serialize(serializer),
        }
    }
}

/// A writer that keeps the first `cap` bytes and fails the write that would pass them, so a
/// serializer driving it stops at once instead of finishing a value nobody will read.
struct Capped {
    bytes: Vec<u8>,
    cap: usize,
}

impl std::io::Write for Capped {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let room = self.cap - self.bytes.len();
        if data.len() > room {
            self.bytes.extend_from_slice(&data[..room]);
            return Err(std::io::Error::new(std::io::ErrorKind::WriteZero, "past the cap"));
        }
        self.bytes.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The compact JSON text of `value` with its strings and keys cut, at most `cap` bytes of it, and
/// whether there was more. The work is bounded by `cap`, whatever `value` holds: the serializer
/// stops at the first write past it. The text of an overflow ends wherever the cap fell, which may
/// be inside a multi-byte character; that part is dropped.
fn clip_text(value: &serde_json::Value, cap: usize) -> (String, bool) {
    let mut out = Capped { bytes: Vec::new(), cap };
    let overflowed = serde_json::to_writer(&mut out, &Clipped(value)).is_err();
    let text = match String::from_utf8(out.bytes) {
        Ok(text) => text,
        Err(e) => {
            let valid = e.utf8_error().valid_up_to();
            let mut bytes = e.into_bytes();
            bytes.truncate(valid);
            String::from_utf8(bytes).unwrap_or_default()
        }
    };
    (text, overflowed)
}

/// The bytes a key costs in a serialized object: its escaped form, the colon and, after the first
/// field, the comma before it.
fn field_overhead(key: &str, first: bool) -> usize {
    serde_json::to_string(key).map_or(MAX_KEY_CHARS * 6 + 2, |k| k.len()) + 1 + usize::from(!first)
}

impl InputChange {
    /// How `cli` differs from what `gate` remembers; `None` when no top-level field differs.
    fn between(gate: &GateInput, cli: &serde_json::Value) -> Option<Self> {
        let mut clipped = false;
        // The braces of the serialized `fields` are part of its bound.
        let total = MAX_CHANGED_FIELDS_BYTES - 2;
        if gate.object != cli.is_object() {
            // Not the same kind of input: nothing to compare field by field, the CLI's is shown whole.
            let mut fields = serde_json::Map::new();
            let room = total.saturating_sub(field_overhead(WHOLE_INPUT_KEY, true));
            fields.insert(WHOLE_INPUT_KEY.to_string(), Self::fit(cli, room, &mut clipped));
            return Some(InputChange {
                fields,
                removed: vec![],
                clipped,
            });
        }
        let mut fields = serde_json::Map::new();
        let mut removed = Vec::new();
        let mut budget = total;
        let theirs = top_level(cli);
        let mut differs = false;
        for (key, value) in &theirs {
            if gate.fields.get(&key_id(key)).map(|held| &held.digest) == Some(&digest(value)) {
                continue;
            }
            differs = true;
            let (key, cut) = clip_chars(key, MAX_KEY_CHARS);
            clipped |= cut;
            let Some(room) = budget.checked_sub(field_overhead(&key, fields.is_empty())) else {
                clipped = true;
                continue;
            };
            // Squeezed into what is left only while nothing has been reported yet: a first field cut
            // short says what differs, a later one squeezed to nothing says nothing.
            let shown = if fields.is_empty() {
                Self::fit(value, room, &mut clipped)
            } else {
                match Self::clip_within(value, room, &mut clipped) {
                    Some(shown) => shown,
                    None => {
                        clipped = true;
                        continue;
                    }
                }
            };
            let cost = field_overhead(&key, fields.is_empty()) + shown.to_string().len();
            if cost > budget {
                clipped = true;
                continue;
            }
            budget -= cost;
            fields.insert(key, shown);
        }
        let ours: std::collections::BTreeSet<FieldId> = theirs.iter().map(|(k, _)| key_id(k)).collect();
        for (id, held) in gate.fields.iter().filter(|(id, _)| !ours.contains(*id)) {
            differs = true;
            if removed.len() < 16 {
                match (id, &held.cut_name) {
                    (_, Some(name)) => {
                        clipped = true;
                        removed.push(name.clone());
                    }
                    (FieldId::Literal(name), None) => removed.push(name.clone()),
                    // Only a long name is keyed by digest, and it always keeps its cut name.
                    (FieldId::Hashed(_), None) => clipped = true,
                }
            } else {
                clipped = true;
            }
        }
        differs.then_some(InputChange {
            fields,
            removed,
            clipped,
        })
    }

    /// `value` with its strings cut ([`MAX_CHANGED_VALUE_CHARS`]) when that serializes into `room`
    /// bytes, else `None`. Walks no more than `room` bytes of `value`.
    fn clip_within(value: &serde_json::Value, room: usize, clipped: &mut bool) -> Option<serde_json::Value> {
        let (_, over) = clip_text(value, room);
        (!over).then(|| clip_value(value, clipped))
    }

    /// `value` with its strings cut ([`MAX_CHANGED_VALUE_CHARS`]) and, when what is left still does
    /// not serialize into `room` bytes, replaced by the start of its own JSON text as a string that
    /// does, so a field that differs is never reported as nothing. The work is bounded by `room`.
    fn fit(value: &serde_json::Value, room: usize, clipped: &mut bool) -> serde_json::Value {
        let (text, over) = clip_text(value, room);
        if !over {
            return clip_value(value, clipped);
        }
        *clipped = true;
        // A string's JSON form is at most 6 bytes a character (`\u00XX`), plus its quotes.
        let chars = (room.saturating_sub(2) / 6).min(MAX_CHANGED_VALUE_CHARS);
        serde_json::Value::String(clip_chars(&text, chars).0)
    }
}

/// The CLI's own refusal of one tool call, as the projection keeps it on that call's record
/// (`AgentDomainEvent::PermissionDenied`). Both fields are the CLI's words, verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallDenial {
    pub reason_type: Option<String>,
    pub reason: Option<String>,
}

/// The first `UngatedCliMode` a session reported, as the projection keeps it. See
/// `AgentSessionProjection::ungated_cli_mode`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UngatedCliModeRecord {
    pub reported: String,
    pub detail: String,
}

/// One slash command an agent advertises. **Untrusted text from the agent**: the producer bounds it
/// (`providers::acp::wire::parse_commands`), and the page draws it as plain text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlashCommand {
    /// The word typed after the `/`, without the slash. Never empty, never holds whitespace.
    pub name: String,
    /// One line saying what the command does; may be empty.
    pub description: String,
    /// What the command takes after its name, when the agent said (ACP `input.hint`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// The latest `AgentDomainEvent::AccountLimits` a session reported, as the projection keeps it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountLimitsRecord {
    pub used_percent: f64,
    pub window_minutes: Option<u32>,
    pub resets_at: Option<i64>,
    pub plan: Option<String>,
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

/// One run of the agent's thinking (`ContentDelta` with [`ContentKind::Thinking`]), with the position it
/// occupies among everything else.
///
/// A collection of its own rather than a kind of `TranscriptMessage`: a reply is markdown the panel
/// renders, while thinking is shown as plain text and only on request, and keeping them apart means no
/// reader of `transcript` can mistake one for the other.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThinkingBlock {
    /// Ordering key, shared with the other collections. See `AgentSessionProjection::apply`.
    ///
    /// The seq of the delta that OPENED this block, never updated as it grows, for the same reason as
    /// `TranscriptMessage::seq`.
    pub seq: u64,
    pub turn_id: String,
    pub text: String,
}

/// The most outcomes of messages put into a running turn a projection remembers; the oldest goes first.
/// The host reads each one within a tick of its fold, so this only bounds a session that steers a lot.
pub const MAX_STEER_OUTCOMES: usize = 256;

/// What became of a message put into the running turn, as the provider reported it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SteerOutcome {
    Delivered,
    Dropped { reason: String },
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
    /// A message put into a running turn that the agent read there (`UserMessageDelivered`): part of
    /// that turn, not the start of a new one. Left out of the serialized form when false, so every
    /// record written before it existed reads the same.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub delivered: bool,
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
    /// Set when the CLI itself refused this call (`AgentDomainEvent::PermissionDenied`), its
    /// auto-mode classifier most often. Absent on every other call, and on every call a history
    /// file stored before the field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub denied: Option<ToolCallDenial>,
    /// Set by [`AgentDomainEvent::ToolInputChanged`]: the CLI's own prompt for this call carried a
    /// different input from the one the gate was asked about for the same `tool_use_id`, so the
    /// gate's answer and a human's approval of the original no longer describe what the CLI was
    /// about to run. Holds only the fields that differ, bounded. Absent when the two agree and
    /// whenever no comparison could be made (no gate request for the id, an input over the key
    /// bound, the legacy backend, which has no CLI prompts); absence never says "same". Stored
    /// with the call, and restored with it (`history::load`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed_input: Option<InputChange>,
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

/// The CLI's own permission prompt, relayed through `provider_permission_prompts`.
///
/// Every v1 session is gated: the CLI runs `default` and a `PreToolUse` hook asks about each
/// call. The CLI can still prompt after the hook allows a call, such as a sensitive-file
/// write under `.git/` or `.claude/`. Neither hook approval nor a session rule silences it.
/// Without someone to answer, the headless CLI refuses the write. With this capability,
/// the prompt arrives as another `PermissionRequested` with a new `permission_id` and is
/// answered through `ResolvePermission`. It may share the gate request's `tool_use_id`,
/// but that is not guaranteed.
///
/// `classify_permission_request` and saved prefix rules judge only the gate's request.
/// `eitri_core::agent_backend` answers the CLI's prompt: bypass answers every one, while
/// Auto uses the human's earlier approval of the same call. The classifier does not answer it.
///
/// Every field comes verbatim from the CLI and may be absent. `reason` is prose for display,
/// never for parsing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderPrompt {
    /// The SDK's `decisionReason`: why the CLI asked, in its own words.
    pub reason: Option<String>,
    /// The SDK's `description`: the CLI's short subject, e.g. `.git/probe`.
    pub description: Option<String>,
    /// The SDK's `blockedPath`: the path that triggered the prompt.
    pub blocked_path: Option<String>,
    /// Present when one of the user's own `permissions.ask` rules forced this prompt. In Auto it is a
    /// card even after the user approved the same call's gate card; bypass answers it like any other.
    pub matched_ask_rule: Option<MatchedAskRule>,
    /// Set, to the raw wire value, when the sidecar named an `origin` this build does not know (a
    /// sidecar newer than this client). In Auto it is a card like one an ask rule forced: what a
    /// future kind of ask means cannot be judged here, so no earlier approval answers it. Bypass
    /// answers it like any other. `None` for every origin this build knows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unrecognized_origin: Option<i32>,
}

impl ProviderPrompt {
    /// Whether, in Auto, this prompt is a card of its own even when the user approved the gate's card
    /// for the same call: the user's own ask rule forced it, its kind is unknown to this build, or it
    /// does not say why it asked. The last case is what a content-scoped ask rule (`Edit(file)`,
    /// `Bash(echo:*)`) looks like: the CLI names only a bare tool-name rule (`matched_ask_rule`) and
    /// leaves its reason out for a rule it matched itself, so such a prompt arrives with no reason and
    /// no matched rule, exactly like any other prompt that explains nothing.
    ///
    /// Bypass does not read it: a bypass tab answers every prompt `allow`, these included, and so do
    /// a bypass entry's `y` and the bypass resync sweep.
    pub fn needs_its_own_card(&self) -> bool {
        self.matched_ask_rule.is_some() || self.unrecognized_origin.is_some() || self.says_nothing_about_why()
    }

    /// No matched ask rule and no reason of the CLI's own (an empty or blank one is none): nothing that
    /// tells a prompt the CLI raised itself from the user's own rule. A blocked path is not a reason: a
    /// path check can sit next to a rule's decision, and the measured sensitive-file prompt carries its
    /// reason and no path.
    fn says_nothing_about_why(&self) -> bool {
        self.matched_ask_rule.is_none() && self.reason.as_deref().is_none_or(|reason| reason.trim().is_empty())
    }

    /// Whose question this is, in the words a card and a row note use: the user's own ask rule when
    /// one forced it; when the CLI gave no reason, "Claude Code asked (maybe your ask rule)", since a
    /// content-scoped ask rule of the user's looks exactly like that; and the neutral "Claude Code
    /// asked" otherwise (a prompt of a kind this build does not know, or one that gave its reason,
    /// which a card shows verbatim): the wire does not say whether a reason came from the CLI's own
    /// check or from a hook of the user's, so the label claims no more.
    pub fn label(&self) -> String {
        if let Some(rule) = &self.matched_ask_rule {
            return format!("your ask rule: {}", rule.display());
        }
        if self.unrecognized_origin.is_none() && self.says_nothing_about_why() {
            return "Claude Code asked (maybe your ask rule)".to_string();
        }
        "Claude Code asked".to_string()
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageInfo {
    pub total_cost_usd: f64,
    /// False when the provider reports tokens but no cost (Codex): `total_cost_usd` is then a
    /// placeholder, and a view must show no cost rather than `$0.00`. Left out of the serialized
    /// form while true, so a Claude report is byte for byte what it was; absent in older JSON, which
    /// reads as true.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub cost_reported: bool,
    /// Legacy's `result.num_turns`; `None` from the sidecar, whose `TurnUsage` has none.
    pub num_turns: Option<u32>,
    /// `None` from legacy, whose `ResultLine` reads no token fields.
    pub tokens: Option<TokenUsage>,
    /// `TurnUsage.model`: the model with the most tokens; `None` when empty.
    pub model: Option<String>,
}

fn yes() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

impl Default for UsageInfo {
    fn default() -> Self {
        Self {
            total_cost_usd: 0.0,
            cost_reported: true,
            num_turns: None,
            tokens: None,
            model: None,
        }
    }
}

/// Eitri's sole product-state authority for one conversation. `pending_permissions`
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
/// A chat moved to the other window is rebuilt the same way: its lifecycle events, one
/// [`AgentDomainEvent::MoveSeeded`] with the settled records, then its running turn's events, all
/// through `apply` ([`AgentSessionProjection::adopt`]).
#[derive(Debug, Clone, Default, Serialize)]
pub struct AgentSessionProjection {
    pub session_id: Option<String>,
    /// The real Claude CLI session UUID -- for the legacy backend this is identical to
    /// `session_id` above (the CLI never distinguishes them); for `ClaudeSidecarProvider` this is
    /// the wire's own `SessionReady.provider_session_id`, genuinely distinct from the sidecar's
    /// internal `session_id`. Needed for `claude --resume <id>` -- not
    /// modeled before this phase per this struct's own established "no field with zero current
    /// consumer" discipline.
    pub provider_session_id: Option<String>,
    pub model: Option<String>,
    pub cwd: Option<String>,
    pub status: ProjectionStatus,
    /// `Some(turn_id)` from a successful `AgentSession::send_turn` until the matching
    /// `TurnCompleted` is folded. A session has at most one active turn, so the caller must
    /// reject a second `send_turn` while this is `Some` rather than queue it silently.
    pub active_turn_id: Option<String>,
    /// Prompts the user sent, in order, each carrying the `seq` that orders it against the other
    /// three collections.
    pub user_prompts: Vec<UserPromptRecord>,
    /// Assistant messages in arrival order, each carrying the `seq` that orders it against
    /// `user_prompts`, `tool_calls` and `pending_permissions`. The four collections stay separate
    /// so consumers can read one kind of item without filtering a sum type. `seq` gives all four
    /// a total order so readers can interleave them exactly, even after a snapshot replaces their state.
    /// The frontend cannot reconstruct arrival order from separate collections without it.
    pub transcript: Vec<TranscriptMessage>,
    pub tool_calls: Vec<ToolCallRecord>,
    pub pending_permissions: HashMap<String, PermissionRequestRecord>,
    /// Turns that ended other than by completing, each where it ended (the fifth collection
    /// ordered by `seq`).
    pub turn_endings: Vec<TurnEndingRecord>,
    /// How the latest turn ended when it did not complete, until the next turn starts: what the
    /// panel's status band shows. `None` while a turn runs and after one that completed.
    pub last_turn_ending: Option<TurnEndingKind>,
    /// The `seq` at which the latest `TurnCompleted` was folded, whatever its outcome, or `None` before
    /// the first. `turn_endings` records only the turns that did not complete; a host that lost the
    /// events to a resync reads here whether a turn it started has since ended. Not serialized: no view
    /// draws it.
    #[serde(skip)]
    pub last_turn_end_seq: Option<u64>,
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
    /// each markdown-parsed IN ISOLATION -- so a fragment like "`eitri_" or "**bold" is not valid
    /// standalone markdown and the formatting of every streamed reply breaks.
    ///
    /// Anything that can only occur BETWEEN assistant messages closes the run: a tool call, a
    /// permission request, a turn boundary, a session event. `Thinking` deliberately does not: an
    /// agent that streams a thought in the middle of a reply (an ACP agent may) would otherwise cut
    /// the reply in two, each half markdown-parsed alone.
    #[serde(skip)]
    pub assistant_message_open: bool,
    /// The agent's thinking, in arrival order, each block carrying the `seq` that orders it against
    /// the other collections. Consecutive thinking deltas are one block (`thinking_open`).
    ///
    /// Not part of a stored history (`history::store` keeps prompts, replies and tool calls) and not
    /// read back from Claude's own transcript, so a resumed Claude conversation has none; an agent whose
    /// history is replayed as events (Codex) has the blocks its replay carries.
    pub thinking: Vec<ThinkingBlock>,
    /// True while the last thing that could end a run of thinking has not happened since a thinking
    /// delta: the next thinking delta CONTINUES the last block. Closed by assistant text and by
    /// everything that closes `assistant_message_open`. On the wire (`thinkingOpen`) for the reason
    /// `assistant_message_open` is: a snapshot can land in the middle of a block, and the panel draws
    /// a block that is still open differently from one that is over.
    #[serde(skip)]
    pub thinking_open: bool,
    /// Strictly increasing by exactly one per `apply` call. What a WebView bridge snapshot's
    /// `throughRevision` reports, and what a later `events{fromRevision, throughRevision, ...}`
    /// batch continues from.
    pub last_revision: u64,
    /// Set once, before any live event is folded, when a resumed conversation was seeded with what
    /// it already said (`history::load`). `None` on every fresh session and on a resume that found
    /// nothing to restore.
    ///
    /// **Not an item and not ordered.** It occupies no `seq`, is never produced by `apply`, and is
    /// not part of the four collections -- it is one statement ABOUT them: how many of them came
    /// off a disk, which disk, and where the live part starts (`upto_seq`). `apply` leaves it
    /// exactly as it found it, which is what makes "the history notice describes the seed" true for
    /// the whole life of the projection rather than only at the moment of loading. The one fold
    /// that sets it is a move's [`AgentDomainEvent::MoveSeeded`], which carries the same notice the
    /// projection it was taken from had, about the same records.
    pub history: Option<HistoryNotice>,
    /// The first `UngatedCliMode` this session reported, or `None`.
    ///
    /// Kept rather than folded away because the event alone does not survive a
    /// `UiDelivery::Resync`: an overflowing UI queue drops its events and the host rebuilds from
    /// this projection, and a tripwire that vanished there would leave an ungated session running
    /// until its next report. `eitri_core`'s pump reads this after every delivery. It changes
    /// nothing a view draws -- the status is untouched and it is not serialized; closing the session
    /// is the host's to do.
    #[serde(skip)]
    pub ungated_cli_mode: Option<UngatedCliModeRecord>,
    /// The latest `AgentDomainEvent::CliPermissionMode` this session reported: `Some("auto")` while
    /// the CLI runs its own auto mode under the gate, `Some("default")` when it fell back, `None`
    /// before the first report and on every session that did not ask for auto. Not serialized: it
    /// changes nothing a view draws, only how the host answers (`cli_runs_auto`).
    #[serde(skip)]
    pub cli_mode_reported: Option<String>,
    /// The first `AgentDomainEvent::PolicyMismatch` this session reported, as `(requested, reported)`.
    /// Kept for the same reason as `ungated_cli_mode`: the event alone does not survive a resync, and
    /// the host's pump and its answer path read this. Not serialized.
    #[serde(skip)]
    pub policy_mismatch: Option<(String, String)>,
    /// The latest `AgentDomainEvent::AccountLimits`, or `None` before the first. Not serialized.
    #[serde(skip)]
    pub account_limits: Option<AccountLimitsRecord>,
    /// The latest `AgentDomainEvent::BackgroundCalls`: how many calls the agent left running after its
    /// turns ended. Zero once the session is over. Not serialized.
    #[serde(skip)]
    pub background_calls: u32,
    /// The latest `AgentDomainEvent::CommandsAvailable`, or `None` before the first. Not serialized
    /// here: the snapshot carries it as `availableCommands`.
    #[serde(skip)]
    pub available_commands: Option<Vec<SlashCommand>>,
    /// `background_calls` as it was when the latest turn ended, which a resync that ends that turn reads.
    /// Not serialized.
    #[serde(skip)]
    pub background_at_last_turn_end: u32,
    /// CLI refusals whose call has not been folded yet, oldest first, by tool-use id, attached when
    /// it is. In the one recorded run (CLI 2.1.284) the refusal followed the assistant message
    /// carrying its call; this keeps a refusal that ever arrives first from being lost. Emptied at
    /// every turn's end and whenever the session ends, since a refusal and its call share a turn;
    /// holds at most [`MAX_HELD_DENIALS`], the oldest dropped first, because a refusal naming a call
    /// that never comes would otherwise sit here for as long as the turn lasts.
    #[serde(skip)]
    pub denials_before_their_call: VecDeque<(String, ToolCallDenial)>,
    /// What each gate request (no provider prompt) carried, by its tool-use id (a digest per field,
    /// not the input), kept after the request is answered so a later CLI prompt for the same call
    /// can be compared with what the gate judged. Newest last; at most [`MAX_HELD_DENIALS`], the
    /// oldest dropped first; emptied where the denials are.
    #[serde(skip)]
    pub gate_inputs: VecDeque<(String, GateInput)>,
    /// Changes whose call has not been folded yet, held and attached like a refusal.
    #[serde(skip)]
    pub changed_inputs_before_their_call: VecDeque<(String, InputChange)>,
    /// Where each tool-use id's first call sits in `tool_calls`, so a completion, refusal or changed
    /// input finds its call without searching every call of the session. Checked on every use and
    /// rebuilt when `tool_calls` was changed other than by `apply` (it is a public field).
    /// Public only so a literal with `..Default::default()` can still be written outside this crate;
    /// nothing outside it can read or change what it holds.
    #[serde(skip)]
    pub call_index: CallIndex,
    /// What became of each message put into the running turn, by the host's client id, oldest first and
    /// at most [`MAX_STEER_OUTCOMES`]. Kept here rather than only in the events so the host still learns
    /// it after a resync dropped them (`eitri_core::tab_set` reads it after every delivery). The
    /// delivered ones are also user prompts; nothing else of it is drawn. Not serialized.
    #[serde(skip)]
    pub steer_outcomes: VecDeque<(String, SteerOutcome)>,
    /// Whether an [`AgentDomainEvent::MoveSeeded`] was taken: a second seed would put records from
    /// two exports side by side. Not serialized. Public only so a literal with `..Default::default()`
    /// can still be written outside this crate.
    #[serde(skip)]
    pub move_seeded: bool,
}

/// [`AgentSessionProjection::call_index`]: the position of each tool-use id's first call, valid for
/// the first `covers` calls. Opaque outside this crate.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CallIndex {
    first: HashMap<String, usize>,
    covers: usize,
}

/// Appends `(id, value)`, replacing an earlier entry for the id and dropping the oldest past
/// [`MAX_HELD_DENIALS`].
fn push_bounded<T>(queue: &mut VecDeque<(String, T)>, id: &str, value: T) {
    queue.retain(|(held, _)| held != id);
    queue.push_back((id.to_string(), value));
    while queue.len() > MAX_HELD_DENIALS {
        queue.pop_front();
    }
}

/// How many refusals that arrived ahead of their call a projection holds at once.
pub const MAX_HELD_DENIALS: usize = 64;

/// Which source supplied the restored history.
///
/// The CLI's transcript is what `claude --resume` in a terminal shows. The host keeps a separate
/// copy, and the two can differ. The panel identifies the source so users can tell which history
/// they are seeing and whether a fallback was used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistorySource {
    /// B: the real Claude CLI's own `<uuid>.jsonl`.
    ClaudeTranscript,
    /// A: `history::store`, Eitri's own copy, used only when B could not be read.
    EitriCopy,
}

/// The information the panel displays about restored history: the conversation, its source,
/// any omissions and the reason for using a fallback. This struct is the sole input to the notice
/// row, which appears whenever history is restored, even if nothing was truncated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// The exclusive `seq` bound of restored history: historical items are below it and live items
    /// are at or above it. Present whenever history is restored, even without truncation, so the
    /// panel can distinguish historical tool calls that cannot complete from live calls still running.
    pub upto_seq: u64,
    /// The file this history was actually read from. Always non-empty: a notice exists only when
    /// history was restored, and history can only come from one file.
    pub source_path: String,
    /// The Claude transcript that was looked for and not used. `None` when `source` is
    /// `ClaudeTranscript` (it would repeat `source_path`) and when no path could be built at all.
    pub attempted_transcript_path: Option<String>,
    /// Why B was not used, in the user's own terms. `Some` exactly when `source` is `EitriCopy`.
    pub fallback_reason: Option<String>,
    /// The CLI version that wrote the transcript, from the file itself. B only -- A has no such
    /// concept. **Not a schema version**; nothing branches on it.
    pub writer_version: Option<String>,
}

impl AgentSessionProjection {
    /// The first call with `tool_use_id`, as a search from the front would find it.
    fn call_mut(&mut self, tool_use_id: &str) -> Option<&mut ToolCallRecord> {
        let index = &mut self.call_index;
        if index.covers != self.tool_calls.len() {
            index.first.clear();
            for (at, call) in self.tool_calls.iter().enumerate() {
                index.first.entry(call.tool_use_id.clone()).or_insert(at);
            }
            index.covers = self.tool_calls.len();
        }
        match index.first.get(tool_use_id) {
            Some(&at) if self.tool_calls[at].tool_use_id == tool_use_id => self.tool_calls.get_mut(at),
            // Not indexed (a refusal ahead of its call, usually), or out of step with a list changed in
            // place: the search the index stands in for, so the answer is always the one it gives. A
            // wrong entry is rebuilt on the next use.
            stale => {
                if stale.is_some() {
                    index.covers = usize::MAX;
                }
                self.tool_calls.iter_mut().find(|c| c.tool_use_id == tool_use_id)
            }
        }
    }

    /// Keeps a refusal whose call has not been folded yet: a second one for the same id replaces
    /// the first, and past [`MAX_HELD_DENIALS`] the oldest is dropped.
    fn hold_denial(&mut self, tool_use_id: String, denial: ToolCallDenial) {
        self.denials_before_their_call.retain(|(id, _)| id != &tool_use_id);
        self.denials_before_their_call.push_back((tool_use_id, denial));
        while self.denials_before_their_call.len() > MAX_HELD_DENIALS {
            self.denials_before_their_call.pop_front();
        }
    }

    /// The session ended while a turn was running and no `TurnCompleted` came for it: one
    /// [`TurnEndingKind::Lost`] item where it stopped. Nothing when no turn was running, so an
    /// ordinary close after a finished turn adds nothing.
    fn record_lost_turn(&mut self, seq: u64) {
        if let Some(turn_id) = &self.active_turn_id {
            self.turn_endings.push(TurnEndingRecord {
                seq,
                turn_id: turn_id.clone(),
                kind: TurnEndingKind::Lost,
                detail: TurnEndDetail::default(),
                resets_at: self.account_limits.as_ref().and_then(|limits| limits.resets_at),
            });
            self.last_turn_ending = Some(TurnEndingKind::Lost);
        }
    }

    /// Remembers what a gate request carried; a second request for the same id replaces it, and an
    /// input too wide to remember ([`GateInput::of`]) leaves the id with nothing to compare.
    fn remember_gate_input(&mut self, tool_use_id: &str, input: &serde_json::Value) {
        match GateInput::of(input) {
            Some(gate) => push_bounded(&mut self.gate_inputs, tool_use_id, gate),
            None => self.gate_inputs.retain(|(id, _)| id != tool_use_id),
        }
    }

    /// The event ingestion folds right after `event`, when `event` is a CLI prompt for a call whose
    /// gate request carried another input: the one place the comparison is made. Nothing to compare
    /// with (no id, no remembered gate input, the gate request itself, equal inputs) is `None`, and
    /// `None` is never read as "the same". Reads only, so it may run before or after the fold of
    /// `event` itself (which changes no gate input for a prompt).
    pub fn derived_after(&self, event: &AgentDomainEvent) -> Option<AgentDomainEvent> {
        let AgentDomainEvent::PermissionRequested {
            tool_use_id: Some(id),
            input,
            provider_prompt: Some(_),
            ..
        } = event
        else {
            return None;
        };
        if id.is_empty() {
            return None;
        }
        let (_, gate) = self.gate_inputs.iter().find(|(held, _)| held == id)?;
        let change = InputChange::between(gate, input)?;
        Some(AgentDomainEvent::ToolInputChanged {
            tool_use_id: id.clone(),
            change,
        })
    }

    /// `apply`, then `apply` of what [`Self::derived_after`] derived from `event`, which is
    /// returned. What ingestion does for every event it folds, for callers (tests, loaders) that
    /// have no delivery queue.
    pub fn apply_with_derived(&mut self, event: &AgentDomainEvent) -> Option<AgentDomainEvent> {
        let derived = self.derived_after(event);
        self.apply(event);
        if let Some(derived) = &derived {
            self.apply(derived);
        }
        derived
    }

    fn take_held_changed_input(&mut self, tool_use_id: &str) -> Option<InputChange> {
        let at = self
            .changed_inputs_before_their_call
            .iter()
            .position(|(id, _)| id == tool_use_id)?;
        self.changed_inputs_before_their_call
            .remove(at)
            .map(|(_, change)| change)
    }

    /// Forgets everything held for calls that have not been folded, and the gate inputs: a prompt
    /// and its gate request share a turn.
    fn clear_held_for_calls(&mut self) {
        self.denials_before_their_call.clear();
        self.gate_inputs.clear();
        self.changed_inputs_before_their_call.clear();
    }

    /// Takes the refusal held for this call, if one arrived ahead of it.
    fn take_held_denial(&mut self, tool_use_id: &str) -> Option<ToolCallDenial> {
        let at = self
            .denials_before_their_call
            .iter()
            .position(|(id, _)| id == tool_use_id)?;
        self.denials_before_their_call.remove(at).map(|(_, denial)| denial)
    }

    fn note_steer_outcome(&mut self, client_message_id: &str, outcome: SteerOutcome) {
        self.steer_outcomes.retain(|(id, _)| id != client_message_id);
        self.steer_outcomes.push_back((client_message_id.to_string(), outcome));
        while self.steer_outcomes.len() > MAX_STEER_OUTCOMES {
            self.steer_outcomes.pop_front();
        }
    }

    /// What the provider reported about the message sent under `client_message_id`, if anything yet.
    pub fn steer_outcome(&self, client_message_id: &str) -> Option<&SteerOutcome> {
        self.steer_outcomes
            .iter()
            .rev()
            .find(|(id, _)| id == client_message_id)
            .map(|(_, outcome)| outcome)
    }

    /// Whether the CLI's latest report on this session says it runs its own `auto` mode under the
    /// gate. False before any report, after a fallback to `default`, and on every session that did
    /// not ask for auto (none of which ever reports one).
    pub fn cli_runs_auto(&self) -> bool {
        self.cli_mode_reported.as_deref() == Some("auto")
    }

    /// The reducer: folds one event's effect into this projection, unconditionally bumping
    /// `last_revision` by exactly one regardless of whether the event has any other observable
    /// effect (an event with a real revision but no other effect -- e.g. a `CliPermissionMode` or a
    /// thinking delta with no text -- is still a real, ordered occurrence a WebView bridge must be
    /// able to skip past correctly when replaying from a revision, not something invisible to the
    /// counter). Pure and side-effect-free -- no I/O -- so it's trivially testable without
    /// spawning anything.
    ///
    /// Ordering: an item created by this call takes `seq = self.last_revision` as read on entry --
    /// the value BEFORE the bump at the bottom. Two consequences the rest of the system relies on.
    /// (1) No two items ever share a `seq`, because no single event creates more than one item and
    /// every call bumps the counter exactly once, so `seq` is a total order over all the
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
                self.thinking_open = false;
                self.last_turn_ending = None;
            }
            AgentDomainEvent::UserPromptSubmitted { text } => {
                self.user_prompts.push(UserPromptRecord {
                    seq,
                    text: text.clone(),
                    delivered: false,
                });
                // Anything that can only occur BETWEEN assistant messages closes the run. A prompt
                // is the clearest such thing: without this, the reply to the next turn is appended
                // to the reply to the last one and they render as a single message.
                self.assistant_message_open = false;
                self.thinking_open = false;
            }
            AgentDomainEvent::UserMessageDelivered {
                client_message_id,
                text,
                ..
            } => {
                self.user_prompts.push(UserPromptRecord {
                    seq,
                    text: text.clone(),
                    delivered: true,
                });
                // The agent read it between two of its own steps, so whatever it said before is over,
                // and so is the thought: the next one sits below this prompt, not in the block above it.
                self.assistant_message_open = false;
                self.thinking_open = false;
                self.note_steer_outcome(client_message_id, SteerOutcome::Delivered);
            }
            AgentDomainEvent::UserMessageDropped {
                client_message_id,
                reason,
            } => {
                self.note_steer_outcome(client_message_id, SteerOutcome::Dropped { reason: reason.clone() });
            }
            AgentDomainEvent::ContentDelta {
                kind: ContentKind::Text,
                text,
                ..
            } => {
                // The reply that follows a thought is a new thing on screen; the next thought is a new
                // block.
                self.thinking_open = false;
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
                turn_id,
                text,
            } => {
                // A delta with no text opens no block: an empty one would be a row that says nothing.
                // It closes nothing either, and still bumps `last_revision` (see the doc above).
                if !text.is_empty() {
                    match self.thinking.last_mut() {
                        // Appending leaves `seq` alone: a block is ordered by where it started.
                        Some(open) if self.thinking_open => open.text.push_str(text),
                        _ => {
                            self.thinking.push(ThinkingBlock {
                                seq,
                                turn_id: turn_id.clone(),
                                text: text.clone(),
                            });
                            self.thinking_open = true;
                        }
                    }
                }
            }
            AgentDomainEvent::AssistantMessageBoundary { .. } => {
                self.assistant_message_open = false;
                self.thinking_open = false;
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
                self.thinking_open = false;
                let denied = self.take_held_denial(tool_use_id);
                let changed_input = self.take_held_changed_input(tool_use_id);
                if self.call_index.covers == self.tool_calls.len() {
                    self.call_index
                        .first
                        .entry(tool_use_id.clone())
                        .or_insert(self.tool_calls.len());
                    self.call_index.covers += 1;
                }
                self.tool_calls.push(ToolCallRecord {
                    seq,
                    turn_id: turn_id.clone(),
                    tool_use_id: tool_use_id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                    result: None,
                    denied,
                    changed_input,
                });
            }
            AgentDomainEvent::ToolCallCompleted {
                tool_use_id,
                content,
                is_error,
                ..
            } => {
                if let Some(call) = self.call_mut(tool_use_id) {
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
                self.thinking_open = false;
                if let Some(id) = tool_use_id.as_deref().filter(|id| !id.is_empty()) {
                    if provider_prompt.is_none() {
                        self.remember_gate_input(id, input);
                    }
                }
                // Keyed by `permission_id`, never by `tool_use_id`: a provider prompt is a SECOND
                // request for a call whose gate request was already answered, and it must land as a
                // card of its own rather than be taken for a duplicate.
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
            AgentDomainEvent::TurnCompleted {
                turn_id,
                outcome,
                usage,
                detail,
                ..
            } => {
                self.background_at_last_turn_end = self.background_calls;
                self.last_turn_end_seq = Some(seq);
                let kind = match outcome {
                    TurnOutcome::Completed => None,
                    TurnOutcome::Interrupted => Some(TurnEndingKind::Interrupted),
                    TurnOutcome::Failed => Some(TurnEndingKind::Failed),
                    TurnOutcome::LimitReached => Some(TurnEndingKind::LimitReached),
                };
                if let Some(kind) = kind {
                    self.turn_endings.push(TurnEndingRecord {
                        seq,
                        turn_id: turn_id.clone(),
                        kind,
                        detail: detail.clone(),
                        resets_at: self.account_limits.as_ref().and_then(|limits| limits.resets_at),
                    });
                    self.last_turn_ending = Some(kind);
                }
                self.active_turn_id = None;
                self.assistant_message_open = false;
                self.thinking_open = false;
                self.clear_held_for_calls();
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
            // not -- and the transcript says so where it stopped (`record_lost_turn`), which states
            // only that it did not finish.
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
                    self.record_lost_turn(seq);
                    self.active_turn_id = None;
                    self.assistant_message_open = false;
                    self.thinking_open = false;
                    self.clear_held_for_calls();
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
                self.record_lost_turn(seq);
                self.active_turn_id = None;
                self.assistant_message_open = false;
                self.thinking_open = false;
                self.clear_held_for_calls();
                self.status = ProjectionStatus::Unavailable { reason: reason.clone() };
            }
            AgentDomainEvent::SessionClosed { reason } => {
                self.record_lost_turn(seq);
                self.active_turn_id = None;
                self.assistant_message_open = false;
                self.thinking_open = false;
                self.clear_held_for_calls();
                self.status = ProjectionStatus::Closed { reason: reason.clone() };
            }
            // The host's answer mode is the tab's (`Tab::mode`), and nothing a provider reports moves
            // it; a report of `default` is therefore nothing to fold.
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
            // The latest report wins: the CLI re-reports its mode at every turn's start.
            AgentDomainEvent::CliPermissionMode { reported } => {
                self.cli_mode_reported = Some(reported.clone());
            }
            // First report wins, like the CLI-mode tripwire: the host fails the tab, and until it has
            // the projection says what it was told.
            AgentDomainEvent::PolicyMismatch { requested, reported } => {
                if self.policy_mismatch.is_none() {
                    self.policy_mismatch = Some((requested.clone(), reported.clone()));
                }
            }
            AgentDomainEvent::BackgroundCalls { running } => self.background_calls = *running,
            AgentDomainEvent::CommandsAvailable { commands } => self.available_commands = Some(commands.clone()),
            // The latest report wins: the agent states the whole window each time.
            AgentDomainEvent::AccountLimits {
                used_percent,
                window_minutes,
                resets_at,
                plan,
            } => {
                self.account_limits = Some(AccountLimitsRecord {
                    used_percent: *used_percent,
                    window_minutes: *window_minutes,
                    resets_at: *resets_at,
                    plan: plan.clone(),
                });
                // The latest turn's ending, if it did not complete and knew no reset time, takes this
                // one: the report that explains the stop can come just after it. One it already had
                // stays, and a turn started since ends that. A resumed session's past ending is never
                // filled: its seed carries no `last_turn_ending` (`resume_external`).
                if self.last_turn_ending.is_some() {
                    if let Some(ending) = self.turn_endings.last_mut() {
                        if ending.resets_at.is_none() {
                            ending.resets_at = *resets_at;
                        }
                    }
                }
            }
            // On the first call with that id, or held until that call is folded. The comparison was
            // made when the prompt was folded (`derived_after`); this only records its result.
            AgentDomainEvent::ToolInputChanged { tool_use_id, change } => match self.call_mut(tool_use_id) {
                Some(call) => call.changed_input = Some(change.clone()),
                None => push_bounded(&mut self.changed_inputs_before_their_call, tool_use_id, change.clone()),
            },
            // On the call it refused, or held until that call is folded. A refusal naming no call
            // has nothing to sit on and changes nothing; the call's own error result still says it.
            AgentDomainEvent::PermissionDenied {
                tool_use_id,
                reason_type,
                reason,
                ..
            } => {
                if let Some(id) = tool_use_id {
                    let denial = ToolCallDenial {
                        reason_type: reason_type.clone(),
                        reason: reason.clone(),
                    };
                    match self.call_mut(id) {
                        Some(call) => call.denied = Some(denial),
                        None => self.hold_denial(id.clone(), denial),
                    }
                }
            }
            // Puts the records in place with their own `seq`s and leaves the revision where the
            // exporting projection had it after the bump below. Never touches the status or any other
            // live declaration: those come from the folds around it. A seed that does not fit (records
            // here already, a second seed, a revision behind this one) changes nothing.
            AgentDomainEvent::MoveSeeded(seed) => {
                if self.seed_fits(seed).is_ok() {
                    self.place_seed(seed);
                }
            }
        }
        self.last_revision += 1;
    }
}

/// Whether the projection's thinking block is open after `event`, given whether it was before: what
/// [`AgentSessionProjection::apply`] does to `thinking_open`, for a provider that has to know whether
/// the thought it is about to send continues the block on screen or starts a new one.
///
/// One difference, on the safe side: a `ResumeOutcome` counts as closing, though one that attached to
/// the session asked for leaves the block open. A provider that takes an open block for closed only
/// leaves out a separator; one that took a closed block for open would start the next block with one.
/// No wildcard arm, so a new event has to say which it is.
pub fn thinking_open_after(open: bool, event: &AgentDomainEvent) -> bool {
    match event {
        // An empty delta opens nothing and closes nothing.
        AgentDomainEvent::ContentDelta {
            kind: ContentKind::Thinking,
            text,
            ..
        } => open || !text.is_empty(),
        AgentDomainEvent::ContentDelta {
            kind: ContentKind::Text,
            ..
        }
        | AgentDomainEvent::TurnStarted { .. }
        | AgentDomainEvent::UserPromptSubmitted { .. }
        | AgentDomainEvent::UserMessageDelivered { .. }
        | AgentDomainEvent::AssistantMessageBoundary { .. }
        | AgentDomainEvent::ToolCallStarted { .. }
        | AgentDomainEvent::PermissionRequested { .. }
        | AgentDomainEvent::TurnCompleted { .. }
        | AgentDomainEvent::ResumeOutcome { .. }
        | AgentDomainEvent::SessionUnavailable { .. }
        | AgentDomainEvent::SessionClosed { .. } => false,
        AgentDomainEvent::SessionOpened { .. }
        | AgentDomainEvent::ToolCallCompleted { .. }
        | AgentDomainEvent::PermissionResolved { .. }
        | AgentDomainEvent::PermissionModeChanged { .. }
        | AgentDomainEvent::UngatedCliMode { .. }
        | AgentDomainEvent::CliPermissionMode { .. }
        | AgentDomainEvent::PermissionDenied { .. }
        | AgentDomainEvent::PolicyMismatch { .. }
        | AgentDomainEvent::BackgroundCalls { .. }
        | AgentDomainEvent::AccountLimits { .. }
        | AgentDomainEvent::ToolInputChanged { .. }
        | AgentDomainEvent::CommandsAvailable { .. }
        // Dropped changes nothing on screen: it only settles a queued message.
        | AgentDomainEvent::UserMessageDropped { .. }
        // A seed sets records, never whether a block is open.
        | AgentDomainEvent::MoveSeeded(_) => open,
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
mod tool_call_lookup {
    use super::*;

    fn fold_calls(count: usize) -> std::time::Duration {
        let mut p = AgentSessionProjection::default();
        let started = std::time::Instant::now();
        for i in 0..count {
            let id = format!("toolu_{i:08}");
            p.apply(&AgentDomainEvent::ToolCallStarted {
                turn_id: "t".into(),
                tool_use_id: id.clone(),
                name: "Read".into(),
                input: serde_json::Value::Null,
            });
            p.apply(&AgentDomainEvent::ToolCallCompleted {
                turn_id: "t".into(),
                tool_use_id: id,
                content: serde_json::Value::Null,
                is_error: false,
            });
        }
        let elapsed = started.elapsed();
        assert!(p.tool_calls.iter().all(|c| c.result.is_some()));
        elapsed
    }

    fn started(id: &str) -> AgentDomainEvent {
        AgentDomainEvent::ToolCallStarted {
            turn_id: "t".into(),
            tool_use_id: id.into(),
            name: "Read".into(),
            input: serde_json::Value::Null,
        }
    }

    fn completed(id: &str, content: &str) -> AgentDomainEvent {
        AgentDomainEvent::ToolCallCompleted {
            turn_id: "t".into(),
            tool_use_id: id.into(),
            content: content.into(),
            is_error: false,
        }
    }

    /// Two calls with one id: the result goes to the first, as a search from the front found it.
    #[test]
    fn a_repeated_id_completes_its_first_call() {
        let mut p = AgentSessionProjection::default();
        p.apply(&started("a"));
        p.apply(&started("b"));
        p.apply(&started("a"));
        p.apply(&completed("a", "first"));
        assert_eq!(p.tool_calls[0].result.as_ref().unwrap().content, "first");
        assert!(p.tool_calls[2].result.is_none());
        assert!(p.tool_calls[1].result.is_none());
    }

    /// The list is public: a call put in or taken out by hand is still found.
    #[test]
    fn calls_changed_by_hand_are_still_found() {
        let mut p = AgentSessionProjection::default();
        p.apply(&started("a"));
        p.apply(&started("b"));
        let mut by_hand = p.tool_calls[0].clone();
        by_hand.tool_use_id = "c".into();
        p.tool_calls.push(by_hand);
        p.apply(&completed("c", "c"));
        assert_eq!(p.tool_calls[2].result.as_ref().unwrap().content, "c");

        p.tool_calls.remove(0);
        p.apply(&completed("b", "b"));
        assert_eq!(p.tool_calls[0].result.as_ref().unwrap().content, "b");

        // Same length, ids changed in place: one the index never saw, one it placed elsewhere.
        p.tool_calls[0].tool_use_id = "d".into();
        p.apply(&completed("d", "d"));
        assert_eq!(p.tool_calls[0].result.as_ref().unwrap().content, "d");
        p.tool_calls[1].tool_use_id = "b".into();
        p.apply(&completed("b", "b again"));
        assert_eq!(p.tool_calls[1].result.as_ref().unwrap().content, "b again");

        // A clone carries an index that still answers for it.
        let mut copy = p.clone();
        copy.apply(&started("e"));
        copy.apply(&completed("e", "e"));
        assert_eq!(copy.tool_calls[2].result.as_ref().unwrap().content, "e");
    }

    /// Completing a call does not search every call before it: folding eight times as many calls
    /// takes about eight times as long, not sixty-four.
    #[test]
    fn folding_completions_scales_linearly() {
        let best = |count| (0..3).map(|_| fold_calls(count)).min().unwrap();
        let small = best(2_000);
        let large = best(16_000);
        eprintln!("2000 calls: {small:?}, 16000 calls: {large:?}");
        assert!(
            large < small * 24,
            "16000 calls took {large:?}, 2000 took {small:?}: worse than linear"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A resync that ends a turn reads the count the turn ended with, which a later report must not move.
    #[test]
    fn the_background_count_at_a_turn_end_is_kept() {
        let mut p = AgentSessionProjection::default();
        p.apply(&AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
        p.apply(&AgentDomainEvent::BackgroundCalls { running: 2 });
        p.apply(&AgentDomainEvent::TurnCompleted {
            turn_id: "t1".into(),
            outcome: TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
            detail: TurnEndDetail::default(),
        });
        p.apply(&AgentDomainEvent::BackgroundCalls { running: 0 });
        assert_eq!((p.background_calls, p.background_at_last_turn_end), (0, 2));
    }

    fn limits(resets_at: i64) -> AgentDomainEvent {
        AgentDomainEvent::AccountLimits {
            used_percent: 100.0,
            window_minutes: Some(300),
            resets_at: Some(resets_at),
            plan: None,
        }
    }

    fn limit_reached(turn_id: &str) -> AgentDomainEvent {
        AgentDomainEvent::TurnCompleted {
            turn_id: turn_id.into(),
            outcome: TurnOutcome::LimitReached,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
            detail: TurnEndDetail {
                reason: Some("usage_limit".into()),
                api_error_status: None,
                message: None,
            },
        }
    }

    /// An ending keeps the reset time current when it happened; a later report changes the account's
    /// figure, not what the row says. One reported just after the stop fills an ending that had none,
    /// until the next turn starts.
    #[test]
    fn an_ending_keeps_the_reset_time_it_had() {
        let mut p = AgentSessionProjection::default();
        p.apply(&AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
        p.apply(&limits(1000));
        p.apply(&limit_reached("t1"));
        p.apply(&limits(2000));
        assert_eq!(p.turn_endings[0].resets_at, Some(1000));
        assert_eq!(p.account_limits.as_ref().unwrap().resets_at, Some(2000));

        let mut late = AgentSessionProjection::default();
        late.apply(&AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
        late.apply(&limit_reached("t1"));
        assert_eq!(late.turn_endings[0].resets_at, None);
        late.apply(&limits(3000));
        late.apply(&limits(4000));
        assert_eq!(
            late.turn_endings[0].resets_at,
            Some(3000),
            "the first report after the stop"
        );
        late.apply(&AgentDomainEvent::TurnStarted { turn_id: "t2".into() });
        late.apply(&limit_reached("t2"));
        late.apply(&limits(5000));
        assert_eq!(late.turn_endings[0].resets_at, Some(3000));
        assert_eq!(late.turn_endings[1].resets_at, Some(4000));
    }

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

    fn started(id: &str) -> AgentDomainEvent {
        AgentDomainEvent::ToolCallStarted {
            turn_id: "t".into(),
            tool_use_id: id.into(),
            name: "Bash".into(),
            input: serde_json::json!({ "command": "git push --force origin main" }),
        }
    }

    fn denied(id: Option<&str>) -> AgentDomainEvent {
        AgentDomainEvent::PermissionDenied {
            tool_use_id: id.map(Into::into),
            tool_name: "Bash".into(),
            reason_type: Some("classifier".into()),
            reason: Some("[Git Destructive]".into()),
        }
    }

    fn git_destructive() -> Option<ToolCallDenial> {
        Some(ToolCallDenial {
            reason_type: Some("classifier".into()),
            reason: Some("[Git Destructive]".into()),
        })
    }

    /// The CLI's refusal lands on the call it refused, by tool-use id, and on no other; it creates
    /// no item of its own, so the `seq` order is untouched.
    #[test]
    fn a_cli_refusal_lands_on_the_call_it_refused() {
        let mut p = AgentSessionProjection::default();
        p.apply(&started("toolu_a"));
        p.apply(&started("toolu_b"));
        p.apply(&denied(Some("toolu_b")));
        assert_eq!(p.tool_calls[0].denied, None);
        assert_eq!(p.tool_calls[1].denied, git_destructive());
        assert_eq!(p.tool_calls.len(), 2);
        assert_eq!(p.last_revision, 3, "one revision per event, as every event");
        let json = serde_json::to_value(&p.tool_calls[1]).unwrap();
        assert_eq!(json["denied"]["reason"], "[Git Destructive]");
        assert!(
            serde_json::to_value(&p.tool_calls[0]).unwrap().get("denied").is_none(),
            "absent, not null, on a call nobody refused"
        );
    }

    /// A refusal folded before its call is kept until the call arrives, and dropped at the turn's
    /// end if it never does; one naming no call changes nothing.
    #[test]
    fn a_refusal_before_its_call_waits_for_it_and_one_naming_no_call_changes_nothing() {
        let mut p = AgentSessionProjection::default();
        p.apply(&denied(Some("toolu_late")));
        p.apply(&denied(None));
        p.apply(&started("toolu_late"));
        assert_eq!(p.tool_calls[0].denied, git_destructive());
        assert!(p.denials_before_their_call.is_empty());

        p.apply(&denied(Some("toolu_never")));
        p.apply(&AgentDomainEvent::TurnCompleted {
            turn_id: "t".into(),
            outcome: TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
            detail: Default::default(),
        });
        assert!(p.denials_before_their_call.is_empty());
        p.apply(&started("toolu_never"));
        assert_eq!(
            p.tool_calls[1].denied, None,
            "a later turn's call is not the one refused"
        );
    }

    /// Refusals that arrive ahead of their calls are held up to a cap, oldest dropped first, and a
    /// repeat for one id replaces rather than adds.
    #[test]
    fn refusals_held_for_calls_that_never_come_are_capped_oldest_first() {
        let mut p = AgentSessionProjection::default();
        for n in 0..MAX_HELD_DENIALS + 6 {
            p.apply(&denied(Some(&format!("toolu_{n}"))));
        }
        assert_eq!(p.denials_before_their_call.len(), MAX_HELD_DENIALS);
        p.apply(&denied(Some("toolu_69")));
        assert_eq!(
            p.denials_before_their_call.len(),
            MAX_HELD_DENIALS,
            "a repeat adds nothing"
        );
        // The six oldest are gone; the first one kept is still attached to its call, the dropped
        // one is not.
        p.apply(&started("toolu_0"));
        p.apply(&started("toolu_5"));
        p.apply(&started("toolu_6"));
        assert_eq!(p.tool_calls[0].denied, None);
        assert_eq!(p.tool_calls[1].denied, None);
        assert_eq!(p.tool_calls[2].denied, git_destructive());
    }

    /// Whatever ends the turn or the session empties the held refusals: their calls can no longer
    /// arrive, and a stale one must not attach to a later turn's call of the same id.
    #[test]
    fn held_refusals_are_cleared_when_the_turn_or_the_session_ends() {
        let ends: [(&str, AgentDomainEvent); 4] = [
            (
                "turn completed",
                AgentDomainEvent::TurnCompleted {
                    turn_id: "t".into(),
                    outcome: TurnOutcome::Completed,
                    result_text: String::new(),
                    stop_reason: None,
                    usage: None,
                    detail: Default::default(),
                },
            ),
            (
                "session closed",
                AgentDomainEvent::SessionClosed { reason: "bye".into() },
            ),
            (
                "session unavailable",
                AgentDomainEvent::SessionUnavailable { reason: "gone".into() },
            ),
            (
                "a resume that did not continue",
                AgentDomainEvent::ResumeOutcome {
                    requested_provider_session_id: "sess-1".into(),
                    status: ResumeStatus::Rejected,
                    attached_provider_session_id: None,
                    forked: false,
                    detail: None,
                },
            ),
        ];
        for (what, end) in ends {
            let mut p = AgentSessionProjection::default();
            p.apply(&denied(Some("toolu_x")));
            assert_eq!(p.denials_before_their_call.len(), 1, "{what}");
            p.apply(&end);
            assert!(p.denials_before_their_call.is_empty(), "{what}");
            p.apply(&started("toolu_x"));
            assert_eq!(p.tool_calls[0].denied, None, "{what}");
        }
    }

    /// `cli_runs_auto` follows the latest report, and nothing but a report of exactly `auto` sets it.
    #[test]
    fn the_cli_runs_auto_only_while_its_latest_report_says_auto() {
        let mut p = AgentSessionProjection::default();
        assert!(!p.cli_runs_auto(), "no report yet");
        p.apply(&AgentDomainEvent::CliPermissionMode {
            reported: "auto".into(),
        });
        assert!(p.cli_runs_auto());
        p.apply(&AgentDomainEvent::CliPermissionMode {
            reported: "default".into(),
        });
        assert!(!p.cli_runs_auto(), "a fallback to default is not auto");
    }

    fn asked(permission_id: &str, id: Option<&str>, input: serde_json::Value, from_cli: bool) -> AgentDomainEvent {
        AgentDomainEvent::PermissionRequested {
            permission_id: permission_id.into(),
            tool_use_id: id.map(Into::into),
            tool_name: "Bash".into(),
            input,
            provider_prompt: from_cli.then(ProviderPrompt::default),
        }
    }

    fn push(command: &str) -> serde_json::Value {
        serde_json::json!({ "command": command })
    }

    fn change_of(fields: serde_json::Value) -> InputChange {
        InputChange {
            fields: fields.as_object().unwrap().clone(),
            removed: vec![],
            clipped: false,
        }
    }

    /// The measured case: the gate judged `git push -f origin main`, a hook rewrote it, and the CLI's
    /// own prompt carried the rewrite. The comparison is made once, in `derived_after`; the event it
    /// derives is what marks the call, and what the panel is sent.
    #[test]
    fn a_cli_prompt_with_another_input_than_the_gate_saw_derives_the_event_that_marks_the_call() {
        let mut p = AgentSessionProjection::default();
        p.apply(&started("toolu_a"));
        assert_eq!(
            p.apply_with_derived(&asked(
                "g",
                Some("toolu_a"),
                push("git push --force origin main"),
                false
            )),
            None
        );
        p.apply(&AgentDomainEvent::PermissionResolved {
            permission_id: "g".into(),
            outcome: PermissionOutcome::Allowed,
        });
        let derived = p.apply_with_derived(&asked(
            "c",
            Some("toolu_a"),
            push("rtk git push --force origin main"),
            true,
        ));
        assert_eq!(
            derived,
            Some(AgentDomainEvent::ToolInputChanged {
                tool_use_id: "toolu_a".into(),
                change: change_of(push("rtk git push --force origin main")),
            })
        );
        assert_eq!(
            p.tool_calls[0].changed_input,
            Some(change_of(push("rtk git push --force origin main")))
        );
        let json = serde_json::to_value(&p.tool_calls[0]).unwrap();
        assert_eq!(
            json["changed_input"]["fields"],
            push("rtk git push --force origin main")
        );
        assert!(json["changed_input"].get("removed").is_none());
        assert!(json["changed_input"].get("clipped").is_none());
    }

    /// Folding a CLI prompt alone (`apply`) compares nothing: only the derived event marks.
    #[test]
    fn apply_alone_never_marks() {
        let mut p = AgentSessionProjection::default();
        p.apply(&started("toolu_a"));
        p.apply(&asked("g", Some("toolu_a"), push("x"), false));
        p.apply(&asked("c", Some("toolu_a"), push("y"), true));
        assert_eq!(p.tool_calls[0].changed_input, None);
    }

    #[test]
    fn the_same_input_says_nothing_and_key_order_is_not_a_difference() {
        let mut p = AgentSessionProjection::default();
        p.apply(&started("toolu_a"));
        p.apply_with_derived(&asked(
            "g",
            Some("toolu_a"),
            serde_json::json!({ "a": 1, "n": { "x": 1, "y": [1, 2] }, "b": 2 }),
            false,
        ));
        let derived = p.apply_with_derived(&asked(
            "c",
            Some("toolu_a"),
            serde_json::json!({ "b": 2, "n": { "y": [1, 2], "x": 1 }, "a": 1 }),
            true,
        ));
        assert_eq!(derived, None);
        assert_eq!(p.tool_calls[0].changed_input, None);
        assert!(serde_json::to_value(&p.tool_calls[0])
            .unwrap()
            .get("changed_input")
            .is_none());
    }

    /// `1` and `1.0` are different JSON numbers to `Value ==`, and so to the digest.
    #[test]
    fn an_integer_and_a_float_are_different_inputs() {
        let mut p = AgentSessionProjection::default();
        p.apply_with_derived(&asked("g", Some("toolu_a"), serde_json::json!({ "n": 1 }), false));
        let derived = p.derived_after(&asked(
            "c",
            Some("toolu_a"),
            serde_json::from_str(r#"{"n":1.0}"#).unwrap(),
            true,
        ));
        assert!(derived.is_some());
    }

    /// No gate request for the id (never seen, no id at all, another id) is no comparison, and
    /// no comparison is never a mark.
    #[test]
    fn a_comparison_that_cannot_be_made_marks_nothing() {
        let mut p = AgentSessionProjection::default();
        p.apply(&started("toolu_a"));
        for event in [
            asked("c1", Some("toolu_a"), push("x"), true),
            asked("g", None, push("y"), false),
            asked("c2", None, push("z"), true),
            asked("g2", Some("toolu_other"), push("y"), false),
            asked("c3", Some("toolu_a"), push("z"), true),
            asked("g3", Some(""), push("y"), false),
            asked("c4", Some(""), push("z"), true),
        ] {
            assert_eq!(p.apply_with_derived(&event), None);
        }
        assert_eq!(p.tool_calls[0].changed_input, None);
        assert!(p.changed_inputs_before_their_call.is_empty());
    }

    /// A gate request alone (the legacy backend has nothing else) never marks a call.
    #[test]
    fn a_gate_request_alone_marks_nothing() {
        let mut p = AgentSessionProjection::default();
        p.apply(&started("toolu_a"));
        p.apply_with_derived(&asked("g", Some("toolu_a"), push("x"), false));
        assert_eq!(
            p.apply_with_derived(&asked("g", Some("toolu_a"), push("y"), false)),
            None
        );
        assert_eq!(p.tool_calls[0].changed_input, None);
    }

    /// A change that arrives before its call is held and attached when the call starts, and a turn's
    /// end drops both what was held and the gate inputs.
    #[test]
    fn a_change_that_arrives_ahead_of_its_call_is_attached_and_a_turn_end_forgets() {
        let mut p = AgentSessionProjection::default();
        p.apply_with_derived(&asked("g", Some("toolu_a"), push("x"), false));
        p.apply_with_derived(&asked("c", Some("toolu_a"), push("y"), true));
        assert_eq!(p.changed_inputs_before_their_call.len(), 1);
        p.apply(&started("toolu_a"));
        assert_eq!(p.tool_calls[0].changed_input, Some(change_of(push("y"))));
        assert!(p.changed_inputs_before_their_call.is_empty());

        p.apply_with_derived(&asked("g2", Some("toolu_b"), push("x"), false));
        p.apply(&AgentDomainEvent::SessionClosed { reason: "done".into() });
        assert!(p.gate_inputs.is_empty());
        p.apply(&started("toolu_b"));
        assert_eq!(
            p.apply_with_derived(&asked("c2", Some("toolu_b"), push("y"), true)),
            None
        );
        assert_eq!(
            p.tool_calls[1].changed_input, None,
            "the gate input did not outlive the turn"
        );
    }

    #[test]
    fn what_is_held_for_comparison_is_bounded() {
        let mut p = AgentSessionProjection::default();
        for n in 0..(MAX_HELD_DENIALS + 6) {
            p.apply(&asked(&format!("g{n}"), Some(&format!("toolu_{n}")), push("x"), false));
        }
        assert_eq!(p.gate_inputs.len(), MAX_HELD_DENIALS);
        for n in 0..(MAX_HELD_DENIALS + 6) {
            p.apply(&AgentDomainEvent::ToolInputChanged {
                tool_use_id: format!("toolu_{n}"),
                change: change_of(push("y")),
            });
        }
        assert_eq!(p.changed_inputs_before_their_call.len(), MAX_HELD_DENIALS);
    }

    #[test]
    fn a_completed_turn_forgets_the_gate_inputs_and_an_empty_id_is_no_link() {
        let mut p = AgentSessionProjection::default();
        p.apply(&asked("g", Some("toolu_a"), push("x"), false));
        p.apply(&AgentDomainEvent::TurnCompleted {
            turn_id: "t".into(),
            outcome: TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
            detail: TurnEndDetail::default(),
        });
        assert!(p.gate_inputs.is_empty());
        p.apply(&asked("g2", Some(""), push("x"), false));
        assert!(p.gate_inputs.is_empty());
    }

    /// The note and what is stored are bounded: a rewritten `Write` reports only the field that
    /// differs, its text cut, and the gate's own copy of the body is a digest, not the body.
    #[test]
    fn a_large_rewritten_input_is_reported_by_its_differing_field_and_bounded() {
        let body = "a".repeat(100_000);
        let mut p = AgentSessionProjection::default();
        p.apply(&started("toolu_a"));
        let gate = serde_json::json!({ "file_path": "/p/a.rs", "content": body });
        p.apply_with_derived(&asked("g", Some("toolu_a"), gate, false));
        let cli = serde_json::json!({ "file_path": "/p/a.rs", "content": "b".repeat(100_000) });
        let derived = p.apply_with_derived(&asked("c", Some("toolu_a"), cli, true));
        let Some(AgentDomainEvent::ToolInputChanged { change, .. }) = derived else {
            panic!("a rewritten body is a change");
        };
        assert_eq!(
            change.fields.keys().collect::<Vec<_>>(),
            ["content"],
            "the unchanged path is not repeated"
        );
        let shown = change.fields["content"].as_str().unwrap();
        assert_eq!(shown.chars().count(), MAX_CHANGED_VALUE_CHARS + 1);
        assert!(shown.ends_with('…'));
        assert!(change.clipped);
        assert!(serde_json::to_string(&change).unwrap().len() < MAX_CHANGED_FIELDS_BYTES + 200);
    }

    #[test]
    fn many_changed_fields_stop_at_the_byte_bound_and_a_removed_field_is_named() {
        let mut gate = serde_json::Map::new();
        let mut cli = serde_json::Map::new();
        for n in 0..40 {
            gate.insert(format!("k{n:02}"), serde_json::json!("old"));
            cli.insert(format!("k{n:02}"), serde_json::json!("n".repeat(900)));
        }
        gate.insert("dropped".into(), serde_json::json!(1));
        let mut p = AgentSessionProjection::default();
        p.apply(&asked("g", Some("toolu_a"), serde_json::Value::Object(gate), false));
        let change = p
            .derived_after(&asked("c", Some("toolu_a"), serde_json::Value::Object(cli), true))
            .and_then(|e| match e {
                AgentDomainEvent::ToolInputChanged { change, .. } => Some(change),
                _ => None,
            })
            .unwrap();
        assert!(change.clipped);
        assert!(change.fields.len() < 40);
        assert!(serde_json::to_string(&change.fields).unwrap().len() <= MAX_CHANGED_FIELDS_BYTES);
        assert_eq!(change.removed, ["dropped"]);
    }

    /// A removed field alone is a change, with nothing in `fields`; a non-object input is compared
    /// whole, and an input too wide to remember is never compared.
    #[test]
    fn removed_fields_whole_inputs_and_inputs_too_wide_to_remember() {
        let mut p = AgentSessionProjection::default();
        p.apply(&asked(
            "g",
            Some("toolu_a"),
            serde_json::json!({ "a": 1, "b": 2 }),
            false,
        ));
        let Some(AgentDomainEvent::ToolInputChanged { change, .. }) =
            p.derived_after(&asked("c", Some("toolu_a"), serde_json::json!({ "a": 1 }), true))
        else {
            panic!("a dropped field is a change");
        };
        assert!(change.fields.is_empty());
        assert_eq!(change.removed, ["b"]);

        p.apply(&asked("g2", Some("toolu_b"), serde_json::json!("plain"), false));
        assert!(p
            .derived_after(&asked("c2", Some("toolu_b"), serde_json::json!("plain"), true))
            .is_none());
        assert!(p
            .derived_after(&asked("c3", Some("toolu_b"), serde_json::json!("other"), true))
            .is_some());

        let wide: serde_json::Map<_, _> = (0..=MAX_INPUT_KEYS)
            .map(|n| (format!("k{n}"), serde_json::json!(n)))
            .collect();
        p.apply(&asked("g3", Some("toolu_c"), serde_json::Value::Object(wide), false));
        assert!(p.gate_inputs.iter().all(|(id, _)| id != "toolu_c"));
        assert!(p
            .derived_after(&asked("c4", Some("toolu_c"), push("z"), true))
            .is_none());
    }

    /// A key is held by name only while it is short; a long one by its digest, so a hostile key
    /// cannot make the memory of a gate input large, and two long keys sharing a prefix stay apart.
    #[test]
    fn a_long_field_name_is_held_by_digest_and_still_compared_exactly() {
        let long_a = format!("{}a", "k".repeat(MAX_KEY_CHARS + 50));
        let long_b = format!("{}b", "k".repeat(MAX_KEY_CHARS + 50));
        let mut p = AgentSessionProjection::default();
        p.apply(&asked(
            "g",
            Some("toolu_a"),
            serde_json::json!({ long_a.clone(): 1 }),
            false,
        ));
        assert!(p.gate_inputs[0].1.fields.iter().all(|(k, held)| match k {
            FieldId::Literal(name) => name.chars().count() <= MAX_KEY_CHARS,
            FieldId::Hashed(_) => held
                .cut_name
                .as_ref()
                .is_some_and(|n| n.chars().count() <= MAX_KEY_CHARS + 1),
        }));
        assert!(p
            .derived_after(&asked(
                "c1",
                Some("toolu_a"),
                serde_json::json!({ long_a.clone(): 1 }),
                true
            ))
            .is_none());
        let Some(AgentDomainEvent::ToolInputChanged { change, .. }) =
            p.derived_after(&asked("c2", Some("toolu_a"), serde_json::json!({ long_b: 1 }), true))
        else {
            panic!("another long name is a change");
        };
        assert_eq!(change.fields.len(), 1);
        assert_eq!(change.removed.len(), 1, "the dropped long name is reported, clipped");
        assert!(change.clipped);
    }

    fn change_between(gate: serde_json::Value, cli: serde_json::Value) -> Option<InputChange> {
        let mut p = AgentSessionProjection::default();
        p.apply(&asked("g", Some("toolu_a"), gate, false));
        p.derived_after(&asked("c", Some("toolu_a"), cli, true))
            .map(|e| match e {
                AgentDomainEvent::ToolInputChanged { change, .. } => change,
                other => panic!("not a change: {other:?}"),
            })
    }

    fn digest_name(key: &str) -> String {
        use sha2::{Digest, Sha256};
        let hex: String = Sha256::digest(key.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        format!("sha256:{hex}")
    }

    /// A literal field named like a long name's digest is a different field from that long name.
    #[test]
    fn a_field_named_like_a_long_names_digest_is_another_field() {
        let long = "L".repeat(MAX_KEY_CHARS + 10);
        let lookalike = digest_name(&long);
        let same = serde_json::json!({ long.clone(): 1, lookalike.clone(): 2 });
        assert!(
            change_between(same.clone(), same).is_none(),
            "identical inputs are no change"
        );
        let change = change_between(
            serde_json::json!({ long.clone(): 1 }),
            serde_json::json!({ lookalike.clone(): 1 }),
        )
        .expect("the long field was replaced by another one");
        assert_eq!(change.fields.keys().collect::<Vec<_>>(), [lookalike.as_str()]);
        assert_eq!(change.removed.len(), 1);
        let change = change_between(
            serde_json::json!({ lookalike.clone(): 1 }),
            serde_json::json!({ long: 1 }),
        )
        .expect("and the other way round");
        assert_eq!(change.removed, [lookalike]);
    }

    /// A dropped long name is reported by its own cut text, not by the digest the memory keys it by.
    #[test]
    fn a_dropped_long_name_is_reported_by_its_cut_name() {
        let long = format!("{}tail", "n".repeat(MAX_KEY_CHARS + 20));
        let change = change_between(
            serde_json::json!({ long.clone(): 1, "a": 1 }),
            serde_json::json!({ "a": 1 }),
        )
        .expect("a dropped field is a change");
        assert_eq!(change.removed.len(), 1);
        let shown = &change.removed[0];
        assert!(!shown.starts_with("sha256:"), "{shown}");
        assert_eq!(shown.chars().count(), MAX_KEY_CHARS + 1);
        assert!(shown.starts_with(&"n".repeat(MAX_KEY_CHARS)) && shown.ends_with('…'));
        assert!(change.clipped);
    }

    /// Keys that escape to six bytes a character are charged at their serialized size, so the note
    /// never serializes past the bound.
    #[test]
    fn keys_that_escape_are_charged_at_their_serialized_size() {
        let mut gate = serde_json::Map::new();
        let mut cli = serde_json::Map::new();
        for n in 0..19 {
            let key = format!("{}{}", "\u{1}".repeat(MAX_KEY_CHARS - 2), char::from(b'a' + n));
            gate.insert(key.clone(), serde_json::json!(0));
            cli.insert(key, serde_json::json!(1));
        }
        let change = change_between(serde_json::Value::Object(gate), serde_json::Value::Object(cli)).unwrap();
        assert!(!change.fields.is_empty());
        assert!(change.clipped, "not every field fits");
        let size = serde_json::to_string(&change.fields).unwrap().len();
        assert!(size <= MAX_CHANGED_FIELDS_BYTES, "{size} bytes");
    }

    /// The whole of the bound counts, braces and commas included, for many small fields.
    #[test]
    fn many_small_fields_never_serialize_past_the_bound() {
        let mut gate = serde_json::Map::new();
        let mut cli = serde_json::Map::new();
        for n in 0..64 {
            gate.insert(format!("{n:0>60}"), serde_json::json!("old"));
            cli.insert(format!("{n:0>60}"), serde_json::json!("n".repeat(40)));
        }
        let change = change_between(serde_json::Value::Object(gate), serde_json::Value::Object(cli)).unwrap();
        let size = serde_json::to_string(&change.fields).unwrap().len();
        assert!(size <= MAX_CHANGED_FIELDS_BYTES, "{size} bytes");
    }

    /// Change detection compares whole fields, so one digest pass is unavoidable. The cut must stop
    /// at the budget, as checked separately below. Both bounds are relative to work measured in the
    /// same process rather than an absolute time limit.
    #[test]
    fn a_huge_nested_value_is_cut_by_a_walk_bounded_by_the_budget() {
        let before = serde_json::json!({ "a": 1, "list": [] });
        let after = {
            let items: Vec<serde_json::Value> = (0..200_000).map(|n| serde_json::json!({ "i": n, "t": "x" })).collect();
            serde_json::json!({ "a": 1, "list": items.clone(), "more": items })
        };
        let pass = {
            let input = after.clone();
            let started = std::time::Instant::now();
            for value in input.as_object().unwrap().values() {
                std::hint::black_box(digest(value));
            }
            started.elapsed()
        };
        let took = (0..3)
            .map(|_| {
                // Cloning is outside the measurement; the moved input is dropped inside the call.
                let gate = before.clone();
                let cli = after.clone();
                let started = std::time::Instant::now();
                let change = std::hint::black_box(change_between(gate, cli)).unwrap();
                let took = started.elapsed();
                assert!(change.clipped);
                assert!(!change.fields.is_empty());
                assert!(
                    change.fields.contains_key("list"),
                    "the first differing field is cut short, never dropped"
                );
                let size = serde_json::to_string(&change.fields).unwrap().len();
                assert!(size <= MAX_CHANGED_FIELDS_BYTES, "{size} bytes");
                took
            })
            .min()
            .unwrap();
        assert!(took < pass * 4, "took {took:?}, digest pass {pass:?}");
    }

    /// Cutting stops at the budget and must be far cheaper than serializing the whole value,
    /// with both times measured in the same process.
    #[test]
    fn the_cut_of_a_huge_value_is_far_cheaper_than_walking_it() {
        let items: Vec<serde_json::Value> = (0..200_000).map(|n| serde_json::json!({ "i": n, "t": "x" })).collect();
        let value = serde_json::Value::Array(items);
        let started = std::time::Instant::now();
        let text = std::hint::black_box(serde_json::to_string(&value).unwrap());
        let full = started.elapsed();
        drop(text);
        let cut = (0..5)
            .map(|_| {
                let started = std::time::Instant::now();
                let (_text, overflowed) = std::hint::black_box(clip_text(&value, MAX_CHANGED_FIELDS_BYTES));
                let cut = started.elapsed();
                assert!(overflowed);
                cut
            })
            .min()
            .unwrap();
        assert!(cut * 20 < full, "cut {cut:?}, full serialization {full:?}");
    }

    /// The clipping walk stops after about a budget's worth of nodes: it visits a bounded number of
    /// elements of an input it cannot finish.
    #[test]
    fn the_clipping_walk_visits_a_bounded_number_of_nodes() {
        // An iterator-backed count cannot see inside serde_json::Value, so count what is serialized.
        let items: Vec<serde_json::Value> = (0..1_000_000).map(|_| serde_json::json!([])).collect();
        let value = serde_json::Value::Array(items);
        let (text, overflowed) = clip_text(&value, MAX_CHANGED_FIELDS_BYTES);
        assert!(overflowed);
        assert_eq!(text.len(), MAX_CHANGED_FIELDS_BYTES);
    }

    /// One field whose JSON is larger than the budget (control characters are six bytes each) is
    /// reported cut, never as an empty note.
    #[test]
    fn one_field_larger_than_the_budget_is_cut_not_dropped() {
        let mut p = AgentSessionProjection::default();
        p.apply(&asked(
            "g",
            Some("toolu_a"),
            serde_json::json!({ "content": "x" }),
            false,
        ));
        let hostile = "\u{1}".repeat(MAX_CHANGED_VALUE_CHARS);
        let Some(AgentDomainEvent::ToolInputChanged { change, .. }) = p.derived_after(&asked(
            "c",
            Some("toolu_a"),
            serde_json::json!({ "content": hostile }),
            true,
        )) else {
            panic!("a change");
        };
        let shown = change.fields["content"].as_str().expect("a string, cut");
        assert!(!shown.is_empty());
        assert!(change.clipped);
        assert!(serde_json::to_string(&change.fields).unwrap().len() <= MAX_CHANGED_FIELDS_BYTES);
    }

    /// A plain-text input and an object whose only field is named like the whole-input key are not
    /// the same input.
    #[test]
    fn a_whole_input_and_an_object_are_never_taken_for_each_other() {
        let mut p = AgentSessionProjection::default();
        p.apply(&asked("g", Some("toolu_a"), serde_json::json!("plain"), false));
        for cli in [
            serde_json::json!({ "(input)": "plain" }),
            serde_json::json!({ "": "plain" }),
        ] {
            assert!(p.derived_after(&asked("c", Some("toolu_a"), cli, true)).is_some());
        }
        assert!(p
            .derived_after(&asked("c", Some("toolu_a"), serde_json::json!("plain"), true))
            .is_none());
        p.apply(&asked("g2", Some("toolu_b"), serde_json::json!({ "": "plain" }), false));
        assert!(p
            .derived_after(&asked("c2", Some("toolu_b"), serde_json::json!("plain"), true))
            .is_some());
    }
}
