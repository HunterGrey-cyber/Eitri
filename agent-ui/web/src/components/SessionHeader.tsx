import type { AgentUiState } from "../types";

function shortId(id: string | null): string {
  return id === null ? "—" : id.slice(0, 8);
}

export function SessionHeader({ state }: { state: AgentUiState }) {
  const provider = state.provider;
  return (
    <div className="session-header">
      <span className="model">{state.model ?? "no model yet"}</span>
      {/* A terminal status wins over activeTurnId. The reducer now also clears activeTurnId on
          `session_unavailable`/`session_closed` -- the earlier note here argued the opposite, that
          nothing should clear it because no provider event says "that turn is over", and that was
          wrong in its consequence: it left a dead session reading "working" forever, in red, styled
          by the very `.status-unavailable` rule written for the text it was hiding, and it kept
          `App.tsx`'s composer spinner and the supervisor dashboard's Working dot stuck too.
          Clearing it invents no completion; the terminal status is still the thing being shown. */}
      <span
        className={`status status-${state.status.kind}`}
        title={
          state.status.kind === "unavailable" || state.status.kind === "closed"
            ? state.status.reason
            : undefined
        }
      >
        {state.status.kind === "running" && state.activeTurnId !== null ? "working" : state.status.kind}
      </span>
      <span className="backend" title={`backend: ${state.backend}`}>{state.backend}</span>
      {/* Three identities where there really are three. The legacy backend's CLI never separated
          its own session id from Claude's, so `sessionId` and `providerSessionId` are the SAME
          value there -- printing both would invite a reader to conclude the two are distinct and
          happen to match, which is the opposite of true. Collapse it, and say whose id it is.

          A session that has started but has not yet taken a turn has BOTH ids null -- `sessionId
          === providerSessionId` is true there too (null === null), so that state must be checked
          first, or it falls into the "they differ" branch and names a sidecar/claude split that
          was never involved: on the default legacy backend the two ids are always one value, and
          nothing here has split into two processes. Say plainly that neither is assigned yet. */}
      <span
        className="identities"
        title={[
          `conversation (neovibe): ${state.conversationId ?? "n/a"}`,
          `session (verdandi):     ${state.sessionId ?? "not yet assigned"}`,
          `provider (claude):      ${state.providerSessionId ?? "not yet assigned"}`,
        ].join("\n")}
      >
        {state.conversationId !== null && <>conv {shortId(state.conversationId)} · </>}
        {state.sessionId === null && state.providerSessionId === null ? (
          <>session not yet assigned</>
        ) : state.sessionId !== null && state.sessionId === state.providerSessionId ? (
          <>session {shortId(state.sessionId)}</>
        ) : (
          <>
            verdandi {shortId(state.sessionId)} · claude {shortId(state.providerSessionId)}
          </>
        )}
      </span>
      {provider !== null && (
        <span
          className="provider"
          title={[
            `sidecar ${provider.sidecarVersion} · protocol ${provider.protocol}`,
            `claude-agent-sdk ${provider.claudeAgentSdkVersion}`,
            `claude CLI ${provider.claudeCodeVersion}`,
            ...(provider.buildDescription !== null ? [provider.buildDescription] : []),
            ...provider.startupDiagnostics,
          ].join("\n")}
        >
          CLI {provider.claudeCodeVersion}
          {provider.startupDiagnostics.length > 0 ? " ⚠" : ""}
        </span>
      )}
    </div>
  );
}
