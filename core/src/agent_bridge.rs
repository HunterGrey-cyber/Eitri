//! JSON message types and (de)serialization for the Rust<->JS bridge between `shell` and
//! `agent-ui/web`'s embedded frontend, v2 protocol
//! (docs/superpowers/specs/2026-09-09-claude-runtime-provider-refactor-design.md §11): every
//! JS->Rust command carries a `requestId`; Rust replies with exactly one `command_result` per
//! command, and pushes a revisioned `events`/`snapshot` envelope independently of any specific
//! command. See agent-ui/web/src/types.ts for the exact TS-side shapes these must match.

#[cfg(test)]
use agent::AgentSessionProjection;
use agent::{AgentDomainEvent, PermissionMode};
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionModeChoice {
    Auto,
    Bypass,
}

impl From<SessionModeChoice> for PermissionMode {
    fn from(choice: SessionModeChoice) -> Self {
        match choice {
            SessionModeChoice::Auto => PermissionMode::Auto,
            SessionModeChoice::Bypass => PermissionMode::Bypass,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InboundMessage {
    Ready {
        request_id: String,
    },
    /// `resume` carries the Claude provider session id to continue. Absent for a fresh session.
    /// One message rather than two so there is exactly one path into backend construction -- a
    /// second entry point is how a "resume" would eventually acquire its own subtly different
    /// failure handling.
    StartSession {
        request_id: String,
        mode: SessionModeChoice,
        #[serde(default)]
        resume: Option<String>,
    },
    SendMessage {
        request_id: String,
        text: String,
    },
    Interrupt {
        request_id: String,
    },
    /// The WebView reporting how long it took to draw the first assistant text of a turn, measured
    /// from its own receipt of the payload to the animation frame that rendered it.
    ///
    /// A SPAN, not an instant: JS `performance.now()` and Rust `Instant` have unrelated epochs, so a
    /// timestamp crossing this boundary would be a confident, meaningless number. Diagnostic only --
    /// nothing branches on it, and it gets no `command_result`.
    TurnRendered {
        request_id: String,
        receive_to_frame_ms: f64,
    },
    /// "Continue this conversation in a real terminal." Carries nothing of its own: every input the
    /// rule needs already lives in canonical state on the Rust side, and a session id sent from the
    /// frontend would be a second, stale source for the one value that must not be wrong.
    ///
    /// Closes the session before the command is produced -- see `crate::terminal_handoff` for what
    /// this path does and does not claim, and `agent_panel`'s `PendingHandoff` for the ordering.
    HandoffToTerminal {
        request_id: String,
    },
    PermissionResponse {
        request_id: String,
        permission_id: String,
        decision: DecisionChoice,
        /// Only meaningful on a denial -- there is no field anywhere downstream that would show an
        /// approval's reason to the model. Carried flat rather than inside the variant because
        /// `InboundMessage` is already internally tagged on `type`.
        #[serde(default)]
        reason: Option<String>,
    },
    /// `f` in the panel's BROWSE: ask `shell` to start a global HINT.
    HintRequest {
        request_id: String,
    },
    /// The panel's answer to `hint_collect`: how many visible targets it froze for `session_id`.
    HintTargets {
        request_id: String,
        session_id: u64,
        count: usize,
    },
}

/// The decision half of a `permission_response`, as a closed set rather than a bool.
///
/// Typed on the wire so an unrecognized value is a PARSE failure, not a value some later `match`
/// has to give a default to. The dangerous default here is obvious and one-directional: anything
/// that is not clearly a denial must never end up running the tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionChoice {
    Allow,
    Deny,
}

impl DecisionChoice {
    /// Pairs the choice with its reason into the typed decision the backends take. An empty or
    /// whitespace-only reason becomes `None`: sending the model an empty string as its explanation
    /// is worse than sending it nothing.
    pub fn into_decision(self, reason: Option<String>) -> agent::PermissionDecision {
        match self {
            DecisionChoice::Allow => agent::PermissionDecision::Allow,
            DecisionChoice::Deny => agent::PermissionDecision::Deny {
                reason: reason.filter(|r| !r.trim().is_empty()),
            },
        }
    }
}

impl InboundMessage {
    pub fn request_id(&self) -> &str {
        match self {
            InboundMessage::Ready { request_id }
            | InboundMessage::StartSession { request_id, .. }
            | InboundMessage::SendMessage { request_id, .. }
            | InboundMessage::Interrupt { request_id }
            | InboundMessage::TurnRendered { request_id, .. }
            | InboundMessage::HandoffToTerminal { request_id }
            | InboundMessage::PermissionResponse { request_id, .. }
            | InboundMessage::HintRequest { request_id }
            | InboundMessage::HintTargets { request_id, .. } => request_id,
        }
    }
}

/// Parses one JS->Rust message. Never panics -- an unrecognized `type` or malformed JSON becomes
/// `None`, and the caller (`agent_panel.rs`) logs it rather than crashing the whole shell process
/// over one bad message from the WebView. A message that fails to parse has no known `requestId`
/// to reply to, so no `command_result` is possible for it -- this mirrors the pre-v2 behavior
/// exactly (an unparseable message was already silently logged-and-dropped, not surfaced to the
/// frontend).
pub fn parse_inbound_message(json_str: &str) -> Option<InboundMessage> {
    match serde_json::from_str(json_str) {
        Ok(msg) => Some(msg),
        Err(e) => {
            eprintln!("[agent_bridge] failed to parse inbound message: {e} -- raw: {json_str}");
            None
        }
    }
}

/// `{"kind":"command_result","requestId":...,"ok":true}` or
/// `{"kind":"command_result","requestId":...,"ok":false,"error":"..."}`.
pub fn serialize_command_result_for_js(request_id: &str, result: Result<(), &str>) -> String {
    match result {
        Ok(()) => json!({ "kind": "command_result", "requestId": request_id, "ok": true }).to_string(),
        Err(error) => {
            json!({ "kind": "command_result", "requestId": request_id, "ok": false, "error": error }).to_string()
        }
    }
}

/// `{"kind":"hello","backend":...,"permissionModes":[...],"resumableSessions":[...],...}` -- sent once,
/// in reply to the frontend's `ready`, BEFORE any snapshot.
///
/// It exists because the frontend's start screen cannot be honest without it. What a backend can
/// offer is a runtime fact -- which permission policies this client can drive, which Verdandi
/// baseline it expects, which sessions this workspace remembers -- and hardcoding any of it into
/// the frontend would make the frontend wrong for whichever backend it was not written against.
///
/// `permissionModes` is deliberately NOT narrowed by backend name: both backends have a real,
/// separately verified interactive gate, so both currently offer the same two. (This doc previously
/// said "the sidecar path ships BYPASS only in this milestone", which stopped being true when
/// `CLIENT_IMPLEMENTED_PERMISSION_MODES` replaced the per-backend narrowing -- see
/// `agent_backend::tests::both_backends_offer_the_same_permission_policies_because_both_implement_them`.)
///
/// `resumableSessions` is the whole resume gate: one entry per session for which the server
/// advertised resume, this client implements it, AND this workspace has a persisted provider
/// session id. The frontend renders its conversation picker on exactly that array and nothing else,
/// so no row can appear for a workspace with nothing to continue, and an empty array is the normal
/// state rather than a missing one.
///
/// Each entry carries only what a record knows: a provider name, the Claude session id, and the two
/// timestamps. There is no title and no summary anywhere in this payload because there is none on
/// disk -- see `agent::ResumableSession`'s own doc for why the one file that could supply one is
/// deliberately not read. A frontend rendering this must not invent a label for a row.
/// **Correction (2026-09-19): there is a `title` now** -- the first line of the session's first
/// prompt, recorded by this project at write time (`agent::persistence::title_from_prompt`), not read
/// from anyone else's file. It is `null` for a session recorded before titles were kept, and the
/// rule stands for those rows: no label is invented.
/// **Correction (2026-09-20, the owner's ruling): "not read from anyone else's file" is no longer
/// true of this field.** `BackendGreeting::for_kind` fills it from the CLI's own `type:"ai-title"`
/// line where the transcript has one, falling back to the recorded title and then to `null`
/// (`agent::transcript`'s constraint 3). **The frontend is deliberately not told which level a
/// title came from**: source would otherwise leak into rendering, and the whole point of the
/// fallback is that losing the CLI's line returns the picker silently to how it looks today.
/// Nothing about the payload's SHAPE changes, and the no-invented-label rule is untouched.
pub fn serialize_hello_for_js(greeting: &crate::agent_backend::BackendGreeting) -> String {
    json!({
        "kind": "hello",
        "backend": greeting.kind.as_str(),
        "projectDir": greeting.project_dir.to_string_lossy(),
        "permissionModes": greeting.permission_modes,
        // The three-term intersection, already evaluated per session: server-advertised (last
        // known) ∩ client-implemented ∩ this workspace has that persisted provider session.
        // Always an array; empty when any term fails, and the frontend renders its picker rows on
        // exactly this and nothing else.
        "resumableSessions": greeting.resumable.iter().map(|r| json!({
            "provider": r.provider,
            "providerSessionId": r.provider_session_id,
            "createdAt": r.created_at,
            "updatedAt": r.updated_at,
            "title": r.title,
        })).collect::<Vec<_>>(),
        "expectedVerdandiRevision": greeting.expected_verdandi_revision,
    })
    .to_string()
}

/// `{"kind":"theme","vars":{"--nv-bg":"#faf4ed",...}}` -- the complete CSS custom-property set the
/// panel paints with, derived from nvim's highlight groups (`crate::theme`).
///
/// Sent in the `ready` batch right after `hello`, and again whenever nvim's colours change. Always
/// complete, so the frontend's CSS never needs a fallback value of its own. Kept out of `hello`
/// on purpose: `hello` describes the backend, and a theme change must not resend it.
pub fn serialize_theme_for_js(tokens: &crate::theme::ThemeTokens) -> String {
    let vars: serde_json::Map<String, serde_json::Value> = tokens
        .css_vars()
        .into_iter()
        .map(|(name, value)| (name, serde_json::Value::String(value)))
        .collect();
    json!({ "kind": "theme", "vars": vars }).to_string()
}

/// `{"kind":"pane_focus","focused":bool}`: whether the agent panel's pane holds the window's
/// keyboard focus, as `shell`'s GTK focus tracking decides it (`shell::pane_focus`). The panel dims
/// its mode block when this is `false`. `shell` is the source rather than the page's own
/// `window` focus/blur because `shell` decides Ctrl+h/Ctrl+l, and because the same answer also
/// drives the status bar and the pane outline, so the three cannot disagree.
pub fn serialize_pane_focus_for_js(focused: bool) -> String {
    json!({ "kind": "pane_focus", "focused": focused }).to_string()
}

/// `{"kind":"enter_input"}`: the user moved INTO the panel with the keyboard (`Ctrl+l` from the
/// editor), so the panel should open its composer with a blinking caret, the way it did before the
/// three-mode rework made BROWSE the landing mode. The owner asked for exactly that (2026-09-19):
/// "control l 直接闪cursor". A separate envelope rather than a flag on `pane_focus`, because only
/// the keyboard route should do it. A click on a row still lands in BROWSE on that row, as the UI
/// spec says, and `pane_focus` cannot tell the two apart.
pub fn serialize_enter_input_for_js() -> String {
    json!({ "kind": "enter_input" }).to_string()
}

/// `{"kind":"focus_permission"}`: the chat was brought back to answer a card -- its tray chip
/// `agent ⚑N` activated, or `Ctrl+a a` with a card waiting (modules spec §3.3). The panel goes to
/// BROWSE with its cursor on the oldest pending card. `shell` sends it only when the count it
/// keeps (`crate::attention`) is above zero; a panel that finds no card takes the composer instead,
/// as it does for `enter_input`.
pub fn serialize_focus_permission_for_js() -> String {
    json!({ "kind": "focus_permission" }).to_string()
}

/// `{"kind":"select_all"}`: `Ctrl+a Ctrl+a` from `shell`'s prefix while the panel has focus
/// (spec 2026-09-19-window-modes-design.md §3.2). WebKitGTK has no way to be handed the key itself,
/// so the panel does what `Ctrl+a` does in a text field: selects all of the one that has focus, and
/// nothing when none does.
pub fn serialize_select_all_for_js() -> String {
    json!({ "kind": "select_all" }).to_string()
}

/// Global `f` HINT (spec: docs/superpowers/specs/2026-09-19-global-hint-design.md §3.3). `shell`
/// owns the session; these five tell the panel to report, show, narrow, land and clear.
pub fn serialize_hint_collect_for_js(session_id: u64) -> String {
    json!({ "kind": "hint_collect", "sessionId": session_id }).to_string()
}
pub fn serialize_hint_show_for_js(session_id: u64, labels: &[String]) -> String {
    json!({ "kind": "hint_show", "sessionId": session_id, "labels": labels }).to_string()
}
pub fn serialize_hint_prefix_for_js(session_id: u64, typed: &str) -> String {
    json!({ "kind": "hint_prefix", "sessionId": session_id, "typed": typed }).to_string()
}
pub fn serialize_hint_land_for_js(session_id: u64, index: usize) -> String {
    json!({ "kind": "hint_land", "sessionId": session_id, "index": index }).to_string()
}
pub fn serialize_hint_end_for_js(session_id: u64) -> String {
    json!({ "kind": "hint_end", "sessionId": session_id }).to_string()
}

/// `{"kind":"events","fromRevision":...,"throughRevision":...,"events":[<tagged AgentDomainEvent JSON>, ...]}`.
/// `AgentDomainEvent`'s own `#[derive(Serialize)]` produces the tagged shape directly for each
/// element. `from_revision` is the projection's `last_revision` BEFORE this batch was folded;
/// `through_revision` is `last_revision` AFTER. Both are currently informational only -- this
/// phase implements no gap detection, no resync request, and no `RequestSnapshot`-style message
/// type; the frontend never reads `fromRevision`/`throughRevision` at all. The one real reload
/// path that exists today (`shell/MANUAL_VERIFICATION.md`'s "agent-ui verification, protocol v2"
/// section, check 4) works by re-loading the panel's document from scratch and consuming a fresh
/// `snapshot`, not by asking for events from a prior revision -- a real revisioned resync
/// consumer, if one is ever built, is future work, not something already wired up here.
pub fn serialize_events_for_js(from_revision: u64, through_revision: u64, events: &[AgentDomainEvent]) -> String {
    json!({ "kind": "events", "fromRevision": from_revision, "throughRevision": through_revision, "events": events })
        .to_string()
}

/// Everything one snapshot needs, gathered from wherever it actually lives.
///
/// Exists so the serializer stays a pure function of plain data: it can be unit-tested against a
/// hand-built view without constructing a real `AgentBackend`, which would mean spawning a real
/// sidecar process. `SnapshotView::of` is the one place the gathering happens, so the two can never
/// disagree about where a field comes from.
pub struct SnapshotView<'a> {
    pub backend: &'static str,
    pub conversation_id: Option<&'a str>,
    /// Verdandi's session id, from the backend rather than the projection -- the projection does not
    /// learn it until the first `SessionOpened`, which on the sidecar path is not until the first
    /// turn.
    pub session_id: Option<&'a str>,
    pub provider_session_id: Option<String>,
    pub capabilities: agent::ProviderCapabilities,
    pub provider: Option<&'a agent::ProviderInfo>,
    /// Borrowed, not cloned. On the sidecar path this is a live borrow through the ingestion
    /// thread's lock, so a snapshot is serialized directly out of canonical state rather than from a
    /// copy of a whole conversation.
    pub projection: crate::agent_backend::ProjectionRef<'a>,
}

impl<'a> SnapshotView<'a> {
    pub fn of(backend: &'a crate::agent_backend::AgentBackend) -> Self {
        Self {
            backend: backend.kind().as_str(),
            conversation_id: backend.conversation_id(),
            session_id: backend.session_id(),
            provider_session_id: backend.provider_session_id(),
            capabilities: backend.capabilities(),
            provider: backend.provider_info(),
            projection: backend.projection(),
        }
    }
}

/// `{"kind":"snapshot","throughRevision":...,"state":<AgentUiState-shaped JSON>}` -- a
/// hand-written re-shaping of `AgentSessionProjection`'s snake_case Rust fields into the camelCase
/// shape `agent-ui/web/src/types.ts`'s `AgentUiState` expects (deliberately not a direct
/// `#[derive(Serialize)]` passthrough -- the TS and Rust naming conventions differ, and this
/// function is the one place that difference is bridged).
///
/// Takes a `SnapshotView` rather than the projection alone, because two of the three identities
/// live outside it: `conversationId` is Neovibe's own and `providerSessionId` is Claude's, while the
/// projection's `sessionId` is Verdandi's. Collapsing them would defeat the entire point of keeping
/// them apart -- and would eventually send Claude's id back as a session_id, which the sidecar
/// answers with SESSION_NOT_FOUND.
pub fn serialize_snapshot_for_js(view: &SnapshotView<'_>) -> String {
    let projection = &*view.projection;
    // Written out for the same stated reason as `transcript` just below: both field names happen
    // to be single lowercase words today, so the derive would produce the same JSON, but this
    // function exists precisely because Rust and TS name things differently and a field added
    // later would otherwise reach the frontend in whatever spelling Rust used.
    let user_prompts: Vec<Value> = projection
        .user_prompts
        .iter()
        .map(|prompt| json!({ "seq": prompt.seq, "text": prompt.text }))
        .collect();

    // Written out rather than leaning on `TranscriptMessage`'s derive. Both field names happen to
    // be single lowercase words, so the derive would produce the same JSON today -- but this
    // function exists precisely because Rust and TS name things differently, and a field added to
    // that struct later would otherwise reach the frontend in whatever spelling Rust used.
    let transcript: Vec<Value> = projection
        .transcript
        .iter()
        .map(|message| json!({ "seq": message.seq, "text": message.text }))
        .collect();

    let tool_calls: Vec<Value> = projection
        .tool_calls
        .iter()
        .map(|call| {
            json!({
                // Where this call sits among the assistant messages and permission cards. Without
                // it the frontend had three collections and no way to interleave them, so every
                // tool card rendered below every message. See `AgentSessionProjection::apply`.
                "seq": call.seq,
                "toolUseId": call.tool_use_id,
                "name": call.name,
                "input": call.input,
                "result": call.result.as_ref().map(|r| json!({ "content": r.content, "isError": r.is_error })),
            })
        })
        .collect();

    // `toolUseId` is the link back to the tool call a request gates. It stays genuinely nullable
    // even though every path in both backends now reads whatever id its own message carried: the
    // id can still be absent (a `can_use_tool` request without the field, a provider that sends an
    // empty string, which `projection::tool_use_link` turns into `None`), and this pane must render
    // that honestly rather than guessing at the most recent call. Emitted as an explicit `null`
    // rather than omitted, so the frontend can tell "no id was sent for this request" from "this
    // build predates the field".
    //
    // Sorted by `seq`, which is also the order they were requested in. `pending_permissions` is a
    // `HashMap` and `values()` order is unspecified, so without this two snapshots of one state
    // could emit two different card orders.
    let mut pending: Vec<&agent::PermissionRequestRecord> = projection.pending_permissions.values().collect();
    pending.sort_by_key(|p| p.seq);
    let pending_permissions: Vec<Value> = pending
        .into_iter()
        .map(|p| {
            json!({
                // Where this card sits in the conversation. Used when it has no `toolUseId` to
                // anchor it beside its tool call -- which is every card on the legacy backend.
                "seq": p.seq,
                "permissionId": p.permission_id,
                "toolUseId": p.tool_use_id,
                "toolName": p.tool_name,
                "input": p.input,
            })
        })
        .collect();

    let status = match &projection.status {
        agent::ProjectionStatus::Starting => json!({ "kind": "starting" }),
        agent::ProjectionStatus::Running => json!({ "kind": "running" }),
        agent::ProjectionStatus::Unavailable { reason } => json!({ "kind": "unavailable", "reason": reason }),
        agent::ProjectionStatus::Closed { reason } => json!({ "kind": "closed", "reason": reason }),
    };

    let capabilities = view.capabilities;
    let provider = view.provider.map(|info| {
        json!({
            "sidecarVersion": info.sidecar_version,
            "claudeAgentSdkVersion": info.claude_agent_sdk_version,
            "claudeCodeVersion": info.actual_claude_code_version,
            "protocol": format!("{}.{}", info.protocol_major, info.protocol_minor),
            "buildDescription": info.build_description,
            "startupDiagnostics": info.startup_diagnostics,
        })
    });

    // What this conversation was seeded with before it took its first live event, or `null` on a
    // fresh session. Emitted as an explicit `null` rather than omitted, so the panel can tell "this
    // session restored nothing" from "this build predates the field".
    //
    // `uptoSeq` is the only field here the panel needs for anything other than its notice row: it is
    // the boundary between what was restored and what this session produced, and a tool call below
    // it with no result can never complete -- rendering that as `Running…` would be a spinner on a
    // process that has been gone for days.
    let history = projection.history.as_ref().map(|notice| {
        json!({
            "source": match notice.source {
                agent::HistorySource::ClaudeTranscript => "claude_transcript",
                agent::HistorySource::NeovibeCopy => "neovibe_copy",
            },
            "restoredItems": notice.restored_items,
            "omittedItems": notice.omitted_items,
            "uptoSeq": notice.upto_seq,
            "sourcePath": notice.source_path,
            "attemptedTranscriptPath": notice.attempted_transcript_path,
            "fallbackReason": notice.fallback_reason,
            "writerVersion": notice.writer_version,
        })
    });

    let state = json!({
        "backend": view.backend,
        "history": history,
        // Three identities, three fields. Never one.
        "conversationId": view.conversation_id,
        "sessionId": view.session_id,
        "providerSessionId": view.provider_session_id,
        "model": projection.model,
        "cwd": projection.cwd,
        "userPrompts": user_prompts,
        "transcript": transcript,
        "toolCalls": tool_calls,
        "status": status,
        "activeTurnId": projection.active_turn_id,
        "pendingPermissions": pending_permissions,
        "capabilities": {
            "resume": capabilities.resume,
            "fork": capabilities.fork,
            "interrupt": capabilities.interrupt,
            "bypassPermissionMode": capabilities.bypass_permission_mode,
        },
        "provider": provider,
    });

    json!({ "kind": "snapshot", "throughRevision": projection.last_revision, "state": state }).to_string()
}

/// `{"kind":"handoff","command":...,"cwd":...,"providerSessionId":...}` -- the conversation has
/// been closed here and this is the command that continues it in the user's own terminal.
///
/// Dispatched only AFTER the real session shutdown has completed (`agent_panel`'s
/// `collect_pending_handoff`), so the ordering design doc §8.3 requires -- close and flush before
/// the CLI starts -- holds by construction rather than by hope: the command has not been shown yet
/// while Neovibe is still driving the session.
///
/// `providerSessionId` is read back out of the command's own argv rather than passed in beside it,
/// so the id the panel displays and the id the command resumes are the same value by construction.
///
/// **It carries no claim of exclusivity, because there is none to make.** Nothing was spawned and
/// no lease was taken; the frontend renders the concurrency warning design doc §8.3's closing
/// paragraph requires of this path, and §8.5/§17.7 reject a stronger claim even where a lease IS
/// held.
pub fn serialize_handoff_for_js(command: &agent::handoff::ClaudeResumeCommand) -> String {
    json!({
        "kind": "handoff",
        "command": command.shell_command_line(),
        "cwd": command.cwd(),
        // Read back out of the command's own argv by `ClaudeResumeCommand::provider_session_id`,
        // which is total rather than indexing -- a malformed argv cannot panic the whole shell
        // process over one envelope.
        "providerSessionId": command.provider_session_id(),
    })
    .to_string()
}

/// `{"kind":"error","message":<message>}` -- a fatal, session-ending failure the frontend cannot
/// otherwise detect (e.g. `AgentSession::start` failing because `claude` isn't on `PATH`).
/// Distinct from `command_result`'s `ok:false` (which reports one command's own failure without
/// ending the session) -- this envelope means the whole `AgentSession` is gone.
pub fn serialize_error_for_js(message: &str) -> String {
    json!({ "kind": "error", "message": message }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::PermissionOutcome;

    #[test]
    fn a_theme_envelope_carries_every_css_variable_verbatim() {
        let tokens = crate::theme::ThemeTokens::fallback();
        let value: serde_json::Value = serde_json::from_str(&serialize_theme_for_js(&tokens)).unwrap();
        assert_eq!(value["kind"], "theme");
        let vars = value["vars"].as_object().unwrap();
        assert_eq!(vars.len(), tokens.css_vars().len());
        assert_eq!(vars["--nv-bg"], tokens.bg.hex());
        assert_eq!(vars["--nv-font-prose"], crate::theme::tokens::PROSE_FONT_STACK);
    }

    #[test]
    fn pane_focus_is_a_kind_tagged_boolean() {
        for focused in [true, false] {
            let value: serde_json::Value = serde_json::from_str(&serialize_pane_focus_for_js(focused)).unwrap();
            assert_eq!(value, serde_json::json!({ "kind": "pane_focus", "focused": focused }));
        }
    }

    #[test]
    fn serializes_enter_input() {
        let value: serde_json::Value = serde_json::from_str(&serialize_enter_input_for_js()).unwrap();
        assert_eq!(value, serde_json::json!({ "kind": "enter_input" }));
    }

    #[test]
    fn serializes_focus_permission() {
        let value: serde_json::Value = serde_json::from_str(&serialize_focus_permission_for_js()).unwrap();
        assert_eq!(value, serde_json::json!({ "kind": "focus_permission" }));
    }

    #[test]
    fn serializes_select_all() {
        let value: serde_json::Value = serde_json::from_str(&serialize_select_all_for_js()).unwrap();
        assert_eq!(value, serde_json::json!({ "kind": "select_all" }));
    }

    #[test]
    fn serializes_the_five_hint_envelopes() {
        let v = |s: String| serde_json::from_str::<serde_json::Value>(&s).unwrap();
        assert_eq!(
            v(serialize_hint_collect_for_js(7)),
            serde_json::json!({ "kind": "hint_collect", "sessionId": 7 })
        );
        assert_eq!(
            v(serialize_hint_show_for_js(7, &["a".into(), "s".into()])),
            serde_json::json!({ "kind": "hint_show", "sessionId": 7, "labels": ["a", "s"] })
        );
        assert_eq!(
            v(serialize_hint_prefix_for_js(7, "a")),
            serde_json::json!({ "kind": "hint_prefix", "sessionId": 7, "typed": "a" })
        );
        assert_eq!(
            v(serialize_hint_land_for_js(7, 2)),
            serde_json::json!({ "kind": "hint_land", "sessionId": 7, "index": 2 })
        );
        assert_eq!(
            v(serialize_hint_end_for_js(7)),
            serde_json::json!({ "kind": "hint_end", "sessionId": 7 })
        );
    }

    #[test]
    fn parses_the_two_hint_messages() {
        let m = parse_inbound_message(r#"{"type":"hint_request","request_id":"r1"}"#).unwrap();
        assert_eq!(m.request_id(), "r1");
        assert!(matches!(m, InboundMessage::HintRequest { .. }));
        let m = parse_inbound_message(r#"{"type":"hint_targets","request_id":"r2","session_id":7,"count":3}"#).unwrap();
        assert!(matches!(
            m,
            InboundMessage::HintTargets {
                session_id: 7,
                count: 3,
                ..
            }
        ));
    }

    #[test]
    fn parses_ready_with_request_id() {
        let msg = parse_inbound_message(r#"{"type":"ready","request_id":"r1"}"#).unwrap();
        // .request_id() (a &self borrow) is checked before the by-value `matches!` pattern below,
        // which binds `request_id: String` out of `msg` by value (InboundMessage isn't Copy) --
        // reversing this order would partial-move `msg` and fail to borrow-check on the next line.
        assert_eq!(msg.request_id(), "r1");
        assert!(matches!(msg, InboundMessage::Ready { request_id } if request_id == "r1"));
    }

    #[test]
    fn parses_start_session_with_auto_mode() {
        let msg = parse_inbound_message(r#"{"type":"start_session","request_id":"r2","mode":"auto"}"#).unwrap();
        assert!(matches!(
            msg,
            InboundMessage::StartSession {
                mode: SessionModeChoice::Auto,
                ..
            }
        ));
    }

    #[test]
    fn parses_send_message() {
        let msg = parse_inbound_message(r#"{"type":"send_message","request_id":"r3","text":"hello"}"#).unwrap();
        match msg {
            InboundMessage::SendMessage { text, .. } => assert_eq!(text, "hello"),
            other => panic!("expected SendMessage, got {other:?}"),
        }
    }

    #[test]
    fn parses_interrupt() {
        let msg = parse_inbound_message(r#"{"type":"interrupt","request_id":"r4"}"#).unwrap();
        assert!(matches!(msg, InboundMessage::Interrupt { .. }));
    }

    #[test]
    fn parses_an_approval_which_carries_no_reason() {
        let msg = parse_inbound_message(
            r#"{"type":"permission_response","request_id":"r5","permission_id":"p1","decision":"allow"}"#,
        )
        .unwrap();
        match msg {
            InboundMessage::PermissionResponse {
                permission_id,
                decision,
                reason,
                ..
            } => {
                assert_eq!(permission_id, "p1");
                assert_eq!(decision, DecisionChoice::Allow);
                assert_eq!(decision.into_decision(reason), agent::PermissionDecision::Allow);
            }
            other => panic!("expected PermissionResponse, got {other:?}"),
        }
    }

    #[test]
    fn parses_a_denial_and_carries_its_reason_to_the_model() {
        let msg = parse_inbound_message(
            r#"{"type":"permission_response","request_id":"r5","permission_id":"p1","decision":"deny","reason":"not in this repo"}"#,
        )
        .unwrap();
        match msg {
            InboundMessage::PermissionResponse { decision, reason, .. } => {
                assert_eq!(
                    decision.into_decision(reason),
                    agent::PermissionDecision::Deny {
                        reason: Some("not in this repo".into())
                    }
                );
            }
            other => panic!("expected PermissionResponse, got {other:?}"),
        }
    }

    /// An empty reason box is not a reason. Sending the model "" as its explanation is worse than
    /// sending it nothing, because nothing at least reads as "no reason given".
    #[test]
    fn a_blank_deny_reason_becomes_no_reason_at_all() {
        let msg = parse_inbound_message(
            r#"{"type":"permission_response","request_id":"r5","permission_id":"p1","decision":"deny","reason":"   "}"#,
        )
        .unwrap();
        match msg {
            InboundMessage::PermissionResponse { decision, reason, .. } => {
                assert_eq!(
                    decision.into_decision(reason),
                    agent::PermissionDecision::Deny { reason: None }
                );
            }
            other => panic!("expected PermissionResponse, got {other:?}"),
        }
    }

    /// The failure direction that matters: a decision this build does not recognize must be
    /// REJECTED, never defaulted. A `bool allow` field could not express this -- any unknown value
    /// would have had to become one of the two, and the tempting default is the one that runs the
    /// tool.
    #[test]
    fn an_unrecognized_decision_is_rejected_rather_than_defaulted() {
        assert!(parse_inbound_message(
            r#"{"type":"permission_response","request_id":"r5","permission_id":"p1","decision":"allow_for_session"}"#
        )
        .is_none());
        assert!(
            parse_inbound_message(
                r#"{"type":"permission_response","request_id":"r5","permission_id":"p1","allow":true}"#
            )
            .is_none(),
            "the old bool shape must not still be accepted"
        );
    }

    #[test]
    fn unparseable_json_returns_none_not_a_panic() {
        assert!(parse_inbound_message("not json at all {{{").is_none());
    }

    #[test]
    fn unrecognized_type_returns_none_not_a_panic() {
        assert!(parse_inbound_message(r#"{"type":"some_future_message_type","request_id":"r6"}"#).is_none());
    }

    #[test]
    fn missing_request_id_returns_none_not_a_panic() {
        assert!(parse_inbound_message(r#"{"type":"ready"}"#).is_none());
    }

    #[test]
    fn serialize_command_result_ok() {
        let json_str = serialize_command_result_for_js("r1", Ok(()));
        let parsed: Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed["kind"], "command_result");
        assert_eq!(parsed["requestId"], "r1");
        assert_eq!(parsed["ok"], true);
    }

    #[test]
    fn serialize_command_result_error() {
        let json_str = serialize_command_result_for_js("r1", Err("boom"));
        let parsed: Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed["ok"], false);
        assert_eq!(parsed["error"], "boom");
    }

    #[test]
    fn serialize_events_for_js_carries_revision_range_and_tagged_events() {
        let events = vec![AgentDomainEvent::PermissionResolved {
            permission_id: "p1".into(),
            outcome: PermissionOutcome::Allowed,
        }];
        let json_str = serialize_events_for_js(3, 4, &events);
        let parsed: Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed["kind"], "events");
        assert_eq!(parsed["fromRevision"], 3);
        assert_eq!(parsed["throughRevision"], 4);
        assert_eq!(parsed["events"][0]["type"], "permission_resolved");
        assert_eq!(parsed["events"][0]["permission_id"], "p1");
    }

    #[test]
    fn serialize_error_for_js_wraps_in_kind_error_envelope() {
        let json_str = serialize_error_for_js("failed to start session: no such file or directory");
        let parsed: Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed["kind"], "error");
        assert_eq!(parsed["message"], "failed to start session: no such file or directory");
    }

    #[test]
    fn serialize_snapshot_for_js_produces_camel_case_matching_the_ts_shape() {
        let mut projection = AgentSessionProjection::default();
        projection.apply(&AgentDomainEvent::SessionOpened {
            session_id: "verdandi-1".into(),
            provider_session_id: "claude-1".into(),
            model: "claude-sonnet-5".into(),
            cwd: "/tmp".into(),
        });
        projection.apply(&AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
        projection.apply(&AgentDomainEvent::ToolCallStarted {
            turn_id: "t1".into(),
            tool_use_id: "toolu_1".into(),
            name: "Bash".into(),
            input: json!({"command": "echo hi"}),
        });
        projection.apply(&AgentDomainEvent::ToolCallCompleted {
            turn_id: "t1".into(),
            tool_use_id: "toolu_1".into(),
            content: json!("boom"),
            is_error: true,
        });
        projection.apply(&AgentDomainEvent::SessionUnavailable {
            reason: "provider process exited unexpectedly".into(),
        });

        let provider = agent::ProviderInfo {
            sidecar_version: "0.1.0".into(),
            claude_agent_sdk_version: "0.3.0".into(),
            actual_claude_code_version: "2.1.269".into(),
            protocol_major: 2,
            protocol_minor: 0,
            advertised_capabilities: vec!["handshake".into()],
            advertised_permission_modes: vec!["bypass".into()],
            event_buffer_policy: "bounded-1000".into(),
            build_description: Some("Verdandi checkout: /x @ eb70aa3 (via NEOVIBE_VERDANDI_CHECKOUT)".into()),
            startup_diagnostics: vec!["claude CLI 2.1.269 is untested".into()],
        };
        let view = SnapshotView {
            backend: "sidecar",
            conversation_id: Some("conv-hash"),
            session_id: Some("verdandi-1"),
            provider_session_id: Some("claude-1".to_string()),
            capabilities: agent::ProviderCapabilities {
                resume: false,
                fork: false,
                interrupt: true,
                bypass_permission_mode: true,
                interactive_permission_mode: true,
            },
            provider: Some(&provider),
            projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
        };

        let json_str = serialize_snapshot_for_js(&view);
        let parsed: Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed["kind"], "snapshot");
        assert_eq!(parsed["throughRevision"], 5);
        assert_eq!(parsed["state"]["backend"], "sidecar");
        assert_eq!(parsed["state"]["pendingPermissions"], json!([]));
        assert_eq!(parsed["state"]["status"]["kind"], "unavailable");
        assert_eq!(
            parsed["state"]["status"]["reason"],
            "provider process exited unexpectedly"
        );
        assert_eq!(parsed["state"]["toolCalls"][0]["toolUseId"], "toolu_1");
        assert_eq!(parsed["state"]["toolCalls"][0]["result"]["isError"], true);

        // The three identities reach the frontend as three separate fields. If any two of these
        // ever collapse to the same source, a consumer will eventually send Claude's id where
        // Verdandi's belongs and get SESSION_NOT_FOUND.
        assert_eq!(parsed["state"]["conversationId"], "conv-hash");
        assert_eq!(parsed["state"]["sessionId"], "verdandi-1");
        assert_eq!(parsed["state"]["providerSessionId"], "claude-1");

        assert_eq!(parsed["state"]["capabilities"]["interrupt"], true);
        assert_eq!(
            parsed["state"]["capabilities"]["resume"], false,
            "resume must not be advertised in this milestone"
        );
        assert_eq!(parsed["state"]["provider"]["claudeCodeVersion"], "2.1.269");
        assert_eq!(parsed["state"]["provider"]["protocol"], "2.0");
        assert!(parsed["state"]["provider"]["buildDescription"]
            .as_str()
            .unwrap()
            .contains("eb70aa3"));
        // Warnings only -- a list that is never empty cannot drive a "something is wrong" glyph.
        assert!(parsed["state"]["provider"]["startupDiagnostics"][0]
            .as_str()
            .unwrap()
            .contains("untested"));
    }

    /// The link from a permission card back to the tool call it gates.
    ///
    /// The EVENT path has always carried it (`AgentDomainEvent`'s own derived `Serialize` emits
    /// every field of `PermissionRequested`); this serializer dropped it, so a panel that
    /// rehydrated from a snapshot -- a reload, or a bounded-queue overflow -- lost a link the live
    /// path had. With several calls of one tool in flight, "Bash wants to run" identifies nothing.
    #[test]
    fn a_pending_permission_snapshot_carries_the_tool_call_it_gates() {
        let mut projection = AgentSessionProjection::default();
        projection.apply(&AgentDomainEvent::PermissionRequested {
            permission_id: "perm-1".into(),
            tool_use_id: Some("toolu_01ABC".into()),
            tool_name: "Bash".into(),
            input: json!({"command": "rm -rf /"}),
        });
        let view = SnapshotView {
            backend: "sidecar",
            conversation_id: None,
            session_id: None,
            provider_session_id: None,
            capabilities: agent::ProviderCapabilities {
                resume: false,
                fork: false,
                interrupt: true,
                bypass_permission_mode: true,
                interactive_permission_mode: true,
            },
            provider: None,
            projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
        };
        let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(&view)).unwrap();
        let pending = &parsed["state"]["pendingPermissions"][0];
        assert_eq!(pending["permissionId"], "perm-1");
        assert_eq!(pending["toolUseId"], "toolu_01ABC");
        assert_eq!(pending["toolName"], "Bash");
    }

    /// `null`, not an omitted key. The TS side declares the field as `string | null`, and an absent
    /// key would arrive as `undefined` -- which reads as "this build is too old to send it" rather
    /// than "no id was sent for this request". This pins the serializer's handling of `None`; when
    /// a request genuinely has no id to send is settled upstream, at `agent/src/session.rs`'s
    /// `permission_requested_event` and `agent/src/wire.rs`'s `can_use_tool` arm.
    #[test]
    fn a_permission_with_no_tool_use_id_says_null_rather_than_omitting_the_key() {
        let mut projection = AgentSessionProjection::default();
        projection.apply(&AgentDomainEvent::PermissionRequested {
            permission_id: "perm-1".into(),
            tool_use_id: None,
            tool_name: "Bash".into(),
            input: json!({}),
        });
        let view = SnapshotView {
            backend: "legacy",
            conversation_id: None,
            session_id: None,
            provider_session_id: None,
            capabilities: agent::ProviderCapabilities {
                resume: false,
                fork: false,
                interrupt: true,
                bypass_permission_mode: true,
                interactive_permission_mode: true,
            },
            provider: None,
            projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
        };
        let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(&view)).unwrap();
        let pending = &parsed["state"]["pendingPermissions"][0];
        assert!(pending["toolUseId"].is_null());
        assert!(
            pending.as_object().unwrap().contains_key("toolUseId"),
            "the key must be present and null, not absent"
        );
    }

    /// The notice a restored history puts on the wire (design §5.5). Every key is checked, because
    /// the panel's four wordings each read a different one and a missing key renders as
    /// `undefined` -- which is not a state the TS type admits.
    #[test]
    fn a_restored_history_reaches_the_frontend_as_one_notice_object() {
        let mut projection = AgentSessionProjection::default();
        projection.apply(&AgentDomainEvent::UserPromptSubmitted {
            text: "what did we say?".into(),
        });
        projection.history = Some(agent::HistoryNotice {
            source: agent::HistorySource::NeovibeCopy,
            restored_items: 314,
            omitted_items: Some(1431),
            upto_seq: projection.last_revision,
            source_path: "/state/neovibe/history/conv/sess.json".into(),
            attempted_transcript_path: Some("/claude/projects/p/sess.jsonl".into()),
            fallback_reason: Some("transcript file not found".into()),
            writer_version: None,
        });
        let view = SnapshotView {
            backend: "sidecar",
            conversation_id: None,
            session_id: None,
            provider_session_id: None,
            capabilities: agent::ProviderCapabilities {
                resume: true,
                fork: false,
                interrupt: true,
                bypass_permission_mode: true,
                interactive_permission_mode: true,
            },
            provider: None,
            projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
        };
        let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(&view)).unwrap();
        let history = &parsed["state"]["history"];
        assert_eq!(history["source"], "neovibe_copy");
        assert_eq!(history["restoredItems"], 314);
        assert_eq!(history["omittedItems"], 1431);
        assert_eq!(history["uptoSeq"], 1);
        assert_eq!(history["sourcePath"], "/state/neovibe/history/conv/sess.json");
        assert_eq!(history["attemptedTranscriptPath"], "/claude/projects/p/sess.jsonl");
        assert_eq!(history["fallbackReason"], "transcript file not found");
        assert!(history["writerVersion"].is_null());
        // Every restored item sits below `uptoSeq`, which is what lets the panel tell a historical
        // tool call that can never complete from a live one that is still running (§3.3).
        assert!(parsed["state"]["userPrompts"][0]["seq"].as_u64().unwrap() < history["uptoSeq"].as_u64().unwrap());
    }

    /// A session that restored nothing -- every fresh one -- sends an explicit `null`. Omitting the
    /// key would make "this session has no history" indistinguishable from "this build predates the
    /// field", the same distinction the `toolUseId` test above exists for.
    #[test]
    fn a_session_with_no_restored_history_says_null_rather_than_omitting_the_key() {
        let projection = AgentSessionProjection::default();
        let view = SnapshotView {
            backend: "legacy",
            conversation_id: None,
            session_id: None,
            provider_session_id: None,
            capabilities: agent::ProviderCapabilities {
                resume: false,
                fork: false,
                interrupt: true,
                bypass_permission_mode: true,
                interactive_permission_mode: true,
            },
            provider: None,
            projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
        };
        let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(&view)).unwrap();
        let state = parsed["state"].as_object().unwrap();
        assert!(
            state.contains_key("history"),
            "the key must be present and null, not absent"
        );
        assert!(state["history"].is_null());
    }

    /// The legacy backend's own shape, which the two tests above do not cover between them: on its
    /// `PreToolUse` hook-relay path the permission id and the tool-use id are the SAME string,
    /// because the hook payload's `tool_use_id` is both the identity of the gated call and the key
    /// the live relay socket is filed under. Serializing them as two separate keys with one value
    /// is correct and must stay that way -- the frontend keys cards on `permissionId` and matches
    /// tool calls on `toolUseId`, and collapsing either into the other would tie the routing key to
    /// the rendering link.
    #[test]
    fn a_legacy_hook_relay_permission_sends_the_same_id_under_both_keys() {
        let mut projection = AgentSessionProjection::default();
        projection.apply(&AgentDomainEvent::PermissionRequested {
            permission_id: "toolu_01CtdezhmhUCrBaswxW5HYmC".into(),
            tool_use_id: Some("toolu_01CtdezhmhUCrBaswxW5HYmC".into()),
            tool_name: "Bash".into(),
            input: json!({"command": "echo hello"}),
        });
        let view = SnapshotView {
            backend: "legacy",
            conversation_id: None,
            session_id: None,
            provider_session_id: None,
            capabilities: agent::ProviderCapabilities {
                resume: false,
                fork: false,
                interrupt: true,
                bypass_permission_mode: true,
                interactive_permission_mode: true,
            },
            provider: None,
            projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
        };
        let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(&view)).unwrap();
        let pending = &parsed["state"]["pendingPermissions"][0];
        assert_eq!(pending["permissionId"], "toolu_01CtdezhmhUCrBaswxW5HYmC");
        assert_eq!(pending["toolUseId"], "toolu_01CtdezhmhUCrBaswxW5HYmC");
    }

    /// The snapshot's half of interleaved ordering (2026-09-15).
    ///
    /// The frontend rendered `transcript`, then `toolCalls`, then `pendingPermissions` as three
    /// sequential lists, so every tool card sat below every assistant message. It could not have
    /// done better from this payload: before `seq`, nothing here said how the three collections
    /// interleave. Reconstructing it from the live event stream alone would have been lost on every
    /// reload and every `UiDelivery::Resync`, which both rebuild the whole frontend state from
    /// exactly this snapshot.
    #[test]
    fn a_snapshot_carries_the_order_of_the_three_collections_against_each_other() {
        let mut projection = AgentSessionProjection::default();
        projection.apply(&AgentDomainEvent::ContentDelta {
            turn_id: "t1".into(),
            kind: agent::ContentKind::Text,
            text: "I'll check.".into(),
        });
        projection.apply(&AgentDomainEvent::ToolCallStarted {
            turn_id: "t1".into(),
            tool_use_id: "toolu_1".into(),
            name: "Bash".into(),
            input: json!({}),
        });
        projection.apply(&AgentDomainEvent::ContentDelta {
            turn_id: "t1".into(),
            kind: agent::ContentKind::Text,
            text: "And now this.".into(),
        });
        projection.apply(&AgentDomainEvent::ToolCallStarted {
            turn_id: "t1".into(),
            tool_use_id: "toolu_2".into(),
            name: "Read".into(),
            input: json!({}),
        });
        projection.apply(&AgentDomainEvent::PermissionRequested {
            permission_id: "perm-1".into(),
            tool_use_id: Some("toolu_2".into()),
            tool_name: "Read".into(),
            input: json!({}),
        });

        let view = SnapshotView {
            backend: "sidecar",
            conversation_id: None,
            session_id: None,
            provider_session_id: None,
            capabilities: agent::ProviderCapabilities {
                resume: false,
                fork: false,
                interrupt: true,
                bypass_permission_mode: true,
                interactive_permission_mode: true,
            },
            provider: None,
            projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
        };
        let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(&view)).unwrap();
        let state = &parsed["state"];

        let mut merged: Vec<(u64, String)> = Vec::new();
        for m in state["transcript"].as_array().unwrap() {
            merged.push((
                m["seq"].as_u64().unwrap(),
                format!("text:{}", m["text"].as_str().unwrap()),
            ));
        }
        for c in state["toolCalls"].as_array().unwrap() {
            merged.push((
                c["seq"].as_u64().unwrap(),
                format!("tool:{}", c["toolUseId"].as_str().unwrap()),
            ));
        }
        for p in state["pendingPermissions"].as_array().unwrap() {
            merged.push((
                p["seq"].as_u64().unwrap(),
                format!("perm:{}", p["permissionId"].as_str().unwrap()),
            ));
        }
        merged.sort_by_key(|(seq, _)| *seq);

        assert_eq!(
            merged.into_iter().map(|(_, label)| label).collect::<Vec<_>>(),
            vec![
                "text:I'll check.",
                "tool:toolu_1",
                "text:And now this.",
                "tool:toolu_2",
                "perm:perm-1"
            ],
        );
        // Every seq is below the revision the same envelope reports, so the frontend can seed its
        // own counter from `throughRevision` and never collide with an item this snapshot carried.
        assert_eq!(parsed["throughRevision"], 5);
    }

    /// `pending_permissions` is a `HashMap`, and `HashMap::values()` order is unspecified -- two
    /// snapshots of the same state could emit two different card orders, and a reader sorting by
    /// `seq` would still be at the mercy of whatever order the array happened to arrive in for
    /// anything it could not sort. Emitting them already ordered makes the payload itself
    /// deterministic.
    ///
    /// SIZED AND REPEATED DELIBERATELY, because the thing under test is nondeterminism and a
    /// careless version of this test inherits it. One `HashMap` iterates in one fixed order for its
    /// whole life, and a fresh one draws a new `RandomState`, so what decides whether an UNSORTED
    /// emission passes is how often a fresh map happens to iterate in insertion order. Measured on
    /// this machine, 200,000 fresh maps each: **5 keys reproduce insertion order 2.53% of the
    /// time** -- so the five-key version of this test used to pass roughly one run in forty with
    /// the sort deleted. At 12 keys that fell to 0.001%, and at 16 and 24 keys it was 0 in 200,000.
    ///
    /// So: 16 permissions, and `PROJECTIONS` independently built ones, all of which must agree.
    /// This is not literally deterministic -- nothing that reads a `HashMap` can be -- but a false
    /// pass now needs every one of those independent maps to draw insertion order at a rate already
    /// below what 200,000 trials could measure for a single one. Stated as a measurement rather
    /// than as a guarantee, because that is what it is.
    #[test]
    fn pending_permissions_are_emitted_in_the_order_they_were_requested() {
        const PERMISSIONS: usize = 16;
        const PROJECTIONS: usize = 8;

        let expected: Vec<String> = (0..PERMISSIONS).map(|i| format!("perm-{i:02}")).collect();

        for attempt in 0..PROJECTIONS {
            let mut projection = AgentSessionProjection::default();
            for id in &expected {
                projection.apply(&AgentDomainEvent::PermissionRequested {
                    permission_id: id.clone(),
                    tool_use_id: None,
                    tool_name: "Bash".into(),
                    input: json!({}),
                });
            }
            let view = SnapshotView {
                backend: "legacy",
                conversation_id: None,
                session_id: None,
                provider_session_id: None,
                capabilities: agent::ProviderCapabilities {
                    resume: false,
                    fork: false,
                    interrupt: true,
                    bypass_permission_mode: true,
                    interactive_permission_mode: true,
                },
                provider: None,
                projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
            };
            let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(&view)).unwrap();
            let cards = parsed["state"]["pendingPermissions"].as_array().unwrap();

            let ids: Vec<&str> = cards.iter().map(|p| p["permissionId"].as_str().unwrap()).collect();
            assert_eq!(
                ids, expected,
                "projection {attempt} emitted its cards out of request order"
            );

            // The request order IS seq order; asserted separately so a future change that kept the
            // ids lined up while emitting some other key's order still fails here.
            let seqs: Vec<u64> = cards.iter().map(|p| p["seq"].as_u64().unwrap()).collect();
            assert!(
                seqs.windows(2).all(|w| w[0] < w[1]),
                "projection {attempt} emitted seqs {seqs:?}"
            );
        }
    }

    /// The panel's half of what the user asked (Task 2 of the panel-as-document plan): a snapshot
    /// must carry `user_prompts` too, or a reload/resync loses every prompt the live event stream
    /// already showed. There is no shared `view_of`/`SnapshotView` builder in this module -- every
    /// neighbouring test constructs one inline with the fields it needs and defaults for the rest,
    /// so this follows that pattern rather than inventing a helper.
    #[test]
    fn a_snapshot_carries_the_user_prompts_and_their_seqs() {
        let mut projection = AgentSessionProjection::default();
        projection.apply(&AgentDomainEvent::UserPromptSubmitted { text: "hello".into() });
        let view = SnapshotView {
            backend: "legacy",
            conversation_id: None,
            session_id: None,
            provider_session_id: None,
            capabilities: agent::ProviderCapabilities {
                resume: false,
                fork: false,
                interrupt: true,
                bypass_permission_mode: true,
                interactive_permission_mode: true,
            },
            provider: None,
            projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
        };
        let json: serde_json::Value = serde_json::from_str(&serialize_snapshot_for_js(&view)).unwrap();
        assert_eq!(json["state"]["userPrompts"][0]["text"], "hello");
        assert_eq!(json["state"]["userPrompts"][0]["seq"], 0);
    }

    /// The event path's own half of the same link, pinned here rather than assumed from the derive:
    /// if `PermissionRequested` ever gained a `skip_serializing_if` on this field, the live path
    /// would start disagreeing with the snapshot path and only one of them would be caught.
    #[test]
    fn the_event_path_carries_tool_use_id_too_so_the_two_paths_cannot_diverge() {
        let events = vec![AgentDomainEvent::PermissionRequested {
            permission_id: "perm-1".into(),
            tool_use_id: Some("toolu_01ABC".into()),
            tool_name: "Bash".into(),
            input: json!({}),
        }];
        let parsed: Value = serde_json::from_str(&serialize_events_for_js(0, 1, &events)).unwrap();
        assert_eq!(parsed["events"][0]["tool_use_id"], "toolu_01ABC");
    }

    /// The one inbound message with no `command_result` and nothing branching on it, which is
    /// exactly why it had no test: a silent diagnostic that stops parsing costs a trace column and
    /// nothing else, so nothing would ever report the break. `receive_to_frame_ms` is a SPAN in
    /// milliseconds -- a float, never an instant, since JS `performance.now()` and Rust `Instant`
    /// have unrelated epochs.
    #[test]
    fn parses_turn_rendered_as_a_float_span() {
        let msg =
            parse_inbound_message(r#"{"type":"turn_rendered","request_id":"r7","receive_to_frame_ms":18.5}"#).unwrap();
        assert_eq!(msg.request_id(), "r7");
        match msg {
            InboundMessage::TurnRendered {
                receive_to_frame_ms, ..
            } => assert_eq!(receive_to_frame_ms, 18.5),
            other => panic!("expected TurnRendered, got {other:?}"),
        }
        // An integer on the wire is still a valid span -- JSON has one number type and a whole
        // number of milliseconds is an ordinary measurement, not a different shape.
        let whole =
            parse_inbound_message(r#"{"type":"turn_rendered","request_id":"r8","receive_to_frame_ms":20}"#).unwrap();
        assert!(
            matches!(whole, InboundMessage::TurnRendered { receive_to_frame_ms, .. } if receive_to_frame_ms == 20.0)
        );
        // Missing the measurement is a parse failure, not a defaulted zero: a zero-millisecond
        // render would be reported into a trace as a real, impossibly good number.
        assert!(parse_inbound_message(r#"{"type":"turn_rendered","request_id":"r9"}"#).is_none());
    }

    #[test]
    fn a_legacy_snapshot_carries_no_conversation_id_and_no_provider_block() {
        let projection = AgentSessionProjection::default();
        let view = SnapshotView {
            backend: "legacy",
            conversation_id: None,
            session_id: None,
            provider_session_id: None,
            capabilities: agent::ProviderCapabilities {
                resume: false,
                fork: false,
                interrupt: true,
                bypass_permission_mode: true,
                interactive_permission_mode: true,
            },
            provider: None,
            projection: crate::agent_backend::ProjectionRef::Borrowed(&projection),
        };
        let parsed: Value = serde_json::from_str(&serialize_snapshot_for_js(&view)).unwrap();
        assert_eq!(parsed["state"]["backend"], "legacy");
        assert!(
            parsed["state"]["conversationId"].is_null(),
            "the legacy backend has no conversation identity"
        );
        assert!(parsed["state"]["provider"].is_null());
    }

    #[test]
    fn the_sidecar_hello_carries_its_baseline_and_never_advertises_resume_without_a_record() {
        let greeting = crate::agent_backend::BackendGreeting::for_kind(
            crate::agent_backend::BackendKind::Sidecar,
            std::path::PathBuf::from("/tmp/project"),
        );
        let parsed: Value = serde_json::from_str(&serialize_hello_for_js(&greeting)).unwrap();
        assert_eq!(parsed["kind"], "hello");
        assert_eq!(parsed["backend"], "sidecar");
        assert_eq!(
            parsed["permissionModes"],
            json!(crate::agent_backend::CLIENT_IMPLEMENTED_PERMISSION_MODES),
            "the hello envelope must carry the client's real offer, not a per-backend narrowing"
        );
        assert_eq!(parsed["projectDir"], "/tmp/project");
        assert!(parsed["expectedVerdandiRevision"].is_string());
    }

    /// A greeting with the given sessions, for the hello-envelope tests below.
    fn greeting_with(resumable: Vec<agent::ResumableSession>) -> crate::agent_backend::BackendGreeting {
        crate::agent_backend::BackendGreeting {
            kind: crate::agent_backend::BackendKind::Sidecar,
            project_dir: std::path::PathBuf::from("/tmp/project"),
            permission_modes: &["bypass"],
            expected_verdandi_revision: Some("abc1234"),
            resumable,
        }
    }

    fn resumable(provider_session_id: &str, created_at: &str, updated_at: &str) -> agent::ResumableSession {
        agent::ResumableSession {
            provider: "claude".into(),
            provider_session_id: provider_session_id.into(),
            created_at: created_at.into(),
            updated_at: updated_at.into(),
            title: None,
        }
    }

    /// Every session the workspace can continue reaches the frontend, in the order Rust ranked
    /// them. The frontend renders rows in array order and does no sorting of its own, so this array
    /// IS the picker's order.
    #[test]
    fn hello_lists_every_session_the_workspace_can_continue_in_rank_order() {
        let greeting = greeting_with(vec![
            resumable("1857dcd5-973b-46a2", "1757600000000", "1757700000000"),
            resumable("99b2b206-0000-4000", "1757100000000", "1757200000000"),
        ]);
        let parsed: Value = serde_json::from_str(&serialize_hello_for_js(&greeting)).unwrap();
        let sessions = parsed["resumableSessions"].as_array().expect("an array, always");
        assert_eq!(sessions.len(), 2);
        // The CLAUDE id, not Verdandi's: resuming mints a new Verdandi session, so a record keyed
        // on that one would point at something that stops existing the moment it is used.
        assert_eq!(sessions[0]["providerSessionId"], "1857dcd5-973b-46a2");
        assert_eq!(sessions[0]["provider"], "claude");
        assert_eq!(sessions[0]["createdAt"], "1757600000000");
        assert_eq!(sessions[0]["updatedAt"], "1757700000000");
        assert_eq!(sessions[1]["providerSessionId"], "99b2b206-0000-4000");
    }

    /// Empty, not null or absent: the frontend maps over this field unconditionally, and a workspace
    /// with nothing to continue is a normal state rather than a missing one.
    #[test]
    fn hello_carries_an_empty_list_when_the_workspace_has_nothing_to_continue() {
        let parsed: Value = serde_json::from_str(&serialize_hello_for_js(&greeting_with(Vec::new()))).unwrap();
        assert_eq!(parsed["resumableSessions"], json!([]));
    }

    /// The title crosses as a string, and a session without one crosses as `null` -- present, so the
    /// frontend reads one shape, and not an empty string it might render as a blank row.
    #[test]
    fn hello_carries_each_sessions_title_or_null() {
        let mut titled = resumable("prov-1", "1000", "9000");
        titled.title = Some("fix the picker".into());
        let greeting = greeting_with(vec![titled, resumable("prov-2", "1000", "8000")]);
        let parsed: Value = serde_json::from_str(&serialize_hello_for_js(&greeting)).unwrap();
        assert_eq!(parsed["resumableSessions"][0]["title"], "fix the picker");
        assert_eq!(parsed["resumableSessions"][1]["title"], Value::Null);
    }

    /// Both stamps cross the bridge, because they answer different questions: `createdAt` is when
    /// the conversation began and `updatedAt` is when it was last opened. The frontend shows the
    /// first only when it differs from the second, which it cannot do if only one is sent.
    #[test]
    fn hello_carries_both_stamps_for_each_session() {
        let greeting = greeting_with(vec![resumable("prov-1", "1000", "9000")]);
        let parsed: Value = serde_json::from_str(&serialize_hello_for_js(&greeting)).unwrap();
        assert_eq!(parsed["resumableSessions"][0]["createdAt"], "1000");
        assert_eq!(parsed["resumableSessions"][0]["updatedAt"], "9000");
    }

    #[test]
    fn start_session_parses_with_and_without_a_resume_id() {
        let fresh = parse_inbound_message(r#"{"type":"start_session","request_id":"r1","mode":"bypass"}"#).unwrap();
        match fresh {
            InboundMessage::StartSession { resume, .. } => assert!(resume.is_none()),
            other => panic!("expected StartSession, got {other:?}"),
        }
        let resumed = parse_inbound_message(
            r#"{"type":"start_session","request_id":"r2","mode":"bypass","resume":"claude-abc"}"#,
        )
        .unwrap();
        match resumed {
            InboundMessage::StartSession { resume, .. } => assert_eq!(resume.as_deref(), Some("claude-abc")),
            other => panic!("expected StartSession, got {other:?}"),
        }
    }

    #[test]
    fn parses_handoff_to_terminal() {
        let msg = parse_inbound_message(r#"{"type":"handoff_to_terminal","request_id":"r10"}"#).unwrap();
        assert_eq!(msg.request_id(), "r10");
        assert!(matches!(msg, InboundMessage::HandoffToTerminal { .. }));
    }

    /// The command goes over the wire as one ready-to-run line PLUS its parts. The line is what a
    /// user copies; the parts are what lets the frontend say which session and which directory
    /// without re-parsing the line it was given.
    ///
    /// Built from the real `agent::handoff::ClaudeResumeCommand` rather than a hand-written string,
    /// so a change to what "continue this session" means reaches this envelope automatically.
    #[test]
    fn serialize_handoff_for_js_carries_the_runnable_line_and_its_parts() {
        let command =
            agent::handoff::ClaudeResumeCommand::for_session("/home/user/project", "1857dcd5-973b-46a2").unwrap();
        let parsed: Value = serde_json::from_str(&serialize_handoff_for_js(&command)).unwrap();
        assert_eq!(parsed["kind"], "handoff");
        assert_eq!(
            parsed["command"],
            "cd /home/user/project && claude --resume 1857dcd5-973b-46a2"
        );
        assert_eq!(parsed["cwd"], "/home/user/project");
        assert_eq!(parsed["providerSessionId"], "1857dcd5-973b-46a2");
    }

    /// The id the envelope carries is the ARGUMENT of the command, read back out of the argv --
    /// never a second copy of the id passed in alongside it. Two sources for one value is how the
    /// panel would eventually show one session and print a command resuming another.
    #[test]
    fn the_envelopes_session_id_is_the_one_the_command_actually_resumes() {
        let command = agent::handoff::ClaudeResumeCommand::for_session("/tmp/p", " padded-id ").unwrap();
        let parsed: Value = serde_json::from_str(&serialize_handoff_for_js(&command)).unwrap();
        assert_eq!(parsed["providerSessionId"], "padded-id");
        assert!(parsed["command"]
            .as_str()
            .unwrap()
            .ends_with("claude --resume padded-id"));
    }

    #[test]
    fn the_legacy_hello_keeps_its_real_two_mode_choice() {
        let greeting = crate::agent_backend::BackendGreeting::for_kind(
            crate::agent_backend::BackendKind::Legacy,
            std::path::PathBuf::from("/tmp/project"),
        );
        let parsed: Value = serde_json::from_str(&serialize_hello_for_js(&greeting)).unwrap();
        assert_eq!(parsed["backend"], "legacy");
        assert_eq!(parsed["permissionModes"], json!(["auto", "bypass"]));
        assert_eq!(
            parsed["resumableSessions"],
            json!([]),
            "the legacy backend can never offer a resume"
        );
        assert!(parsed["expectedVerdandiRevision"].is_null());
    }
}
