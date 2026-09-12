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
      };
    case "turn_started":
      return { ...state, activeTurnId: event.turn_id };
    case "content_delta":
      return event.kind === "text" ? { ...state, transcript: [...state.transcript, event.text] } : state;
    case "tool_call_started": {
      const record: ToolCallRecord = { toolUseId: event.tool_use_id, name: event.name, input: event.input, result: null };
      return { ...state, toolCalls: [...state.toolCalls, record] };
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
        pendingPermissions: [
          ...state.pendingPermissions,
          { permissionId: event.permission_id, toolName: event.tool_name, input: event.input },
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
      return { ...state, activeTurnId: null };
    case "session_unavailable":
      return { ...state, status: { kind: "unavailable", reason: event.reason } };
    case "session_closed":
      return { ...state, status: { kind: "closed", reason: event.reason } };
    default: {
      // Exhaustiveness guard: a new AgentDomainEvent variant added on the Rust side without a
      // matching TS case lands here at runtime -- observable, never silently dropped.
      console.warn("agent-ui: completely unhandled event shape from Rust", event);
      return state;
    }
  }
}

export function applySnapshot(_state: AgentUiState, snapshot: AgentUiState): AgentUiState {
  return snapshot;
}
