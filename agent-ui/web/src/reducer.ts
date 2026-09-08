import type { AgentEvent, AgentUiState, ToolCallRecord } from "./types";

export function initialState(): AgentUiState {
  return {
    sessionId: null,
    model: null,
    cwd: null,
    transcript: [],
    toolCalls: [],
    status: { kind: "starting" },
    turnInProgress: false,
    pendingPermissions: [],
  };
}

export function applyEvent(state: AgentUiState, event: AgentEvent): AgentUiState {
  switch (event.type) {
    case "session_started":
      return { ...state, sessionId: event.session_id, model: event.model, cwd: event.cwd, status: { kind: "running" } };
    case "assistant_text":
      return { ...state, transcript: [...state.transcript, event.text] };
    case "thinking":
      return state;
    case "tool_started": {
      const record: ToolCallRecord = { id: event.id, name: event.name, input: event.input, result: null };
      return { ...state, toolCalls: [...state.toolCalls, record] };
    }
    case "tool_result":
      return {
        ...state,
        toolCalls: state.toolCalls.map((call) =>
          call.id === event.id ? { ...call, result: { content: event.content, isError: event.is_error } } : call,
        ),
      };
    case "turn_finished":
      // v2 semantics: a finished turn does NOT end the conversation, only ProcessExited does --
      // matches agent::AgentSessionState::apply exactly. See the agent-v2 spec for why.
      return { ...state, turnInProgress: false };
    case "process_exited":
      if (state.status.kind === "starting" || state.status.kind === "running") {
        return { ...state, status: { kind: "finished", isError: !event.success } };
      }
      return state;
    case "permission_request":
      return {
        ...state,
        pendingPermissions: [
          ...state.pendingPermissions,
          { requestId: event.request_id, toolName: event.tool_name, input: event.input, source: event.source },
        ],
      };
    case "rate_limit":
    case "control_response":
    case "process_stderr":
      return state;
    case "unknown":
      console.warn("agent-ui: unrecognized AgentEvent from Rust", event);
      return state;
    default: {
      // Exhaustiveness guard: a new AgentEvent variant added on the Rust side without a matching
      // TS case lands here at runtime (TypeScript's own exhaustiveness check on `event` would
      // already fail to compile in that case, catching most drift at build time) -- observable,
      // never silently dropped, matching agent::wire's own "never discard" posture.
      console.warn("agent-ui: completely unhandled event shape from Rust", event);
      return state;
    }
  }
}

export function applySnapshot(_state: AgentUiState, snapshot: AgentUiState): AgentUiState {
  return snapshot;
}

// Optimistic, local-only state update for the moment a turn is sent -- there is no AgentEvent
// for "a turn just started" (agent::AgentSession::send_turn sets turn_in_progress on the Rust
// side directly, before any wire event comes back from the CLI at all), so the frontend must
// set this itself right when it calls postToRust({type: "send_message", ...}) rather than
// waiting for something that will never arrive over the event stream. A subsequent
// turn_finished event (or a rehydration snapshot) is what eventually clears it back to false.
export function markTurnStarted(state: AgentUiState): AgentUiState {
  return { ...state, turnInProgress: true };
}

// Optimistic, local-only state update for the moment a permission request is answered -- there is
// no AgentEvent for "a permission request was resolved" (agent::AgentSession::respond_permission
// removes it from the Rust side's own AgentSessionState.pending_permissions directly, but nothing
// on the wire ever announces that removal), so the frontend must clear its own copy itself right
// when it calls postToRust({type: "permission_response", ...}) rather than waiting for an event
// that will never arrive. Without this, an answered PermissionCard stays rendered forever (a real,
// reproducible bug found during Task 8's real sandbox verification -- see
// shell/MANUAL_VERIFICATION.md's "agent-ui verification" section).
export function markPermissionAnswered(state: AgentUiState, requestId: string): AgentUiState {
  return { ...state, pendingPermissions: state.pendingPermissions.filter((p) => p.requestId !== requestId) };
}

// Optimistic, local-only state update for the moment interrupt() is sent -- AgentProcess::interrupt
// now releases (denies) every pending hook connection and AgentSession::interrupt clears
// state.pending_permissions to match, mirroring shutdown()'s own reasoning: a request from the
// turn being interrupted can never be genuinely answered afterward. Nothing on the wire ever
// announces this clear (same gap as markPermissionAnswered's), so without this every permission
// card visible at the moment of interrupt would linger forever, looking actionable when it isn't
// -- found via the interrupt-fix's own real sandbox regression check
// (shell/MANUAL_VERIFICATION.md's "agent-ui verification" section).
export function markTurnInterrupted(state: AgentUiState): AgentUiState {
  return { ...state, pendingPermissions: [] };
}
