import type { AgentDomainEvent, AgentUiState, ToolCallRecord } from "./types";

/** The state before any snapshot arrives. `backend` defaults to "legacy" only because something
 * must be written here -- the real value always arrives with `hello` (before any session can exist)
 * and again with every snapshot, so nothing renders a backend-dependent decision from this default.
 * Capabilities default to all-false, which is the safe direction: a control gated on a capability
 * stays hidden until the server has actually said the capability exists. */
export function initialState(): AgentUiState {
  return {
    backend: "legacy",
    conversationId: null,
    sessionId: null,
    providerSessionId: null,
    model: null,
    cwd: null,
    transcript: [],
    toolCalls: [],
    status: { kind: "starting" },
    activeTurnId: null,
    pendingPermissions: [],
    capabilities: { resume: false, fork: false, interrupt: false, bypassPermissionMode: false },
    provider: null,
    assistantMessageOpen: false,
  };
}

export function applyEvent(state: AgentUiState, event: AgentDomainEvent): AgentUiState {
  switch (event.type) {
    case "session_opened":
      // Arrives once PER TURN on the sidecar backend, not once per session: the Agent SDK emits a
      // system/init at the start of each turn even inside one streaming session. Folding it is
      // idempotent, so that is harmless -- but it must never be read as "a new session began".
      // `session_id` is Verdandi's and `provider_session_id` is Claude's; they are different values
      // and are kept in different fields.
      return {
        ...state,
        sessionId: event.session_id,
        providerSessionId: event.provider_session_id,
        model: event.model,
        cwd: event.cwd,
        status: { kind: "running" },
        assistantMessageOpen: false,
      };
    case "turn_started":
      return { ...state, activeTurnId: event.turn_id, assistantMessageOpen: false };
    case "content_delta": {
      if (event.kind !== "text") return state;
      // `transcript` holds assistant MESSAGES, not content events. Under partial streaming a single
      // 600-word reply arrives as 400+ deltas; pushing each as its own entry renders 400 separate
      // bubbles, each markdown-parsed in isolation -- and a fragment like "`neovibe_" or "**bold"
      // is not valid standalone markdown, so every streamed reply's formatting breaks.
      //
      // MUST stay identical to `AgentSessionProjection::apply` in agent/src/projection.rs: the two
      // fold the same events and a snapshot from Rust has to be indistinguishable from this
      // reducer's own accumulation.
      if (state.assistantMessageOpen && state.transcript.length > 0) {
        const transcript = state.transcript.slice();
        transcript[transcript.length - 1] += event.text;
        return { ...state, transcript };
      }
      return { ...state, transcript: [...state.transcript, event.text], assistantMessageOpen: true };
    }
    case "tool_call_started": {
      const record: ToolCallRecord = { toolUseId: event.tool_use_id, name: event.name, input: event.input, result: null };
      // A tool call only happens between assistant messages, so the streaming text ended here.
      return { ...state, toolCalls: [...state.toolCalls, record], assistantMessageOpen: false };
    }
    case "tool_call_completed":
      return {
        ...state,
        toolCalls: state.toolCalls.map((call) =>
          call.toolUseId === event.tool_use_id ? { ...call, result: { content: event.content, isError: event.is_error } } : call,
        ),
      };
    case "permission_requested":
      return {
        ...state,
        assistantMessageOpen: false,
        pendingPermissions: [
          ...state.pendingPermissions,
          // `tool_use_id` is carried through as-is, null included: it is what ties this card to the
          // exact tool call it gates, and a turn can have several of the same tool in flight. The
          // reducer takes no view on when a request arrives without one -- that is settled
          // upstream, at `agent/src/session.rs`'s `permission_requested_event` -- it only refuses
          // to invent one. Note the two ids may legitimately be equal: on the legacy backend's
          // hook-relay path `permission_id` and `tool_use_id` are the same string, so nothing here
          // may assume they differ.
          {
            permissionId: event.permission_id,
            toolUseId: event.tool_use_id,
            toolName: event.tool_name,
            input: event.input,
          },
        ],
      };
    case "permission_resolved":
      // The one authoritative source for "this permission card is gone" -- design doc §11.2.
      // Replaces the deleted markPermissionAnswered: this now arrives as a real event from Rust
      // the instant AgentSession::respond_permission or interrupt() resolves it, never as a
      // frontend-local guess.
      return { ...state, pendingPermissions: state.pendingPermissions.filter((p) => p.permissionId !== event.permission_id) };
    case "turn_completed":
      // The one authoritative source for "no turn is in flight" -- replaces the deleted
      // markTurnStarted's matching clear. v2 semantics unchanged from v1: a finished turn does
      // NOT end the conversation, only session_closed/session_unavailable do.
      return { ...state, activeTurnId: null, assistantMessageOpen: false };
    // Both endings clear activeTurnId, mirroring AgentSessionProjection exactly: no turn_completed
    // is ever coming, so leaving it set leaves App.tsx's turnInProgress true forever -- a spinner
    // on a dead session, next to a reply that may be truncated. Clearing it is not a local guess at
    // a completion: no message is closed out, no outcome invented; the turn just stops being in
    // flight, which is the truth.
    case "resume_outcome": {
      // Mirrors AgentSessionProjection exactly. A verdict that does not confirm the requested
      // session ends the conversation's usefulness, whatever arrives afterwards: a session that
      // looks continued while carrying none of its history is the one outcome the resume protocol
      // exists to prevent. `forked` is consulted rather than ignored because forking legitimately
      // returns a different id.
      const attached =
        event.status === "attached" &&
        (event.forked || event.attached_provider_session_id === event.requested_provider_session_id);
      if (attached) return state;
      return {
        ...state,
        activeTurnId: null,
        status: { kind: "unavailable", reason: describeFailedResume(event) },
        assistantMessageOpen: false,
      };
    }
    case "session_unavailable":
      return {
        ...state,
        activeTurnId: null,
        status: { kind: "unavailable", reason: event.reason },
        assistantMessageOpen: false,
      };
    case "session_closed":
      return {
        ...state,
        activeTurnId: null,
        status: { kind: "closed", reason: event.reason },
        assistantMessageOpen: false,
      };
    default: {
      // Exhaustiveness guard: a new AgentDomainEvent variant added on the Rust side without a
      // matching TS case lands here at runtime -- observable, never silently dropped.
      console.warn("agent-ui: completely unhandled event shape from Rust", event);
      return state;
    }
  }
}

export function applySnapshot(_state: AgentUiState, snapshot: AgentUiState): AgentUiState {
  // A snapshot is a complete replacement, but it carries no `assistantMessageOpen` -- that flag is
  // reducer-internal on both sides and deliberately not on the wire. Resetting it is the safe
  // direction: the next content event starts a new transcript entry rather than appending to a
  // message that may have been closed before the snapshot was taken.
  return { ...snapshot, assistantMessageOpen: false };
}

/** The user-facing reason a resume did not continue the session that was asked for.
 *
 * Deliberately a near-transcription of Rust's `describe_failed_resume`: the two run on the same
 * events and must not disagree about what happened. Kept as text rather than a code so the panel
 * has something to show without a second mapping table on this side. */
function describeFailedResume(event: Extract<AgentDomainEvent, { type: "resume_outcome" }>): string {
  const detail = event.detail !== null && event.detail.trim() !== "" ? ` (${event.detail})` : "";
  const requested = event.requested_provider_session_id;
  switch (event.status) {
    case "rejected":
      return `the provider does not have session ${requested} any more, so that conversation cannot be continued${detail}. Start a new session instead.`;
    case "initialization_failed":
      return `the provider failed to start while continuing session ${requested}${detail}. This is a provider problem rather than a missing conversation — the session may still exist. Try again, or start a new session.`;
    case "attached":
      return `asked to continue session ${requested}, but the provider attached to ${event.attached_provider_session_id ?? "an unnamed session"} instead, so this conversation carries none of the history that was asked for${detail}. Start a new session instead.`;
  }
}
