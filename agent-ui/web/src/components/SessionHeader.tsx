import type { AgentUiState } from "../types";

function shortId(id: string | null): string {
  return id === null ? "—" : id.slice(0, 8);
}

export function SessionHeader({ state }: { state: AgentUiState }) {
  const provider = state.provider;
  return (
    <div className="session-header">
      <span className="model">{state.model ?? "no model yet"}</span>
      {/* A terminal status wins over activeTurnId. Nothing clears activeTurnId when a session dies
          mid-turn -- correctly so, since no provider event says "that turn is over" -- so without
          this guard a crashed session renders the word "working" forever, in red, styled by the
          very `.status-unavailable` rule written for the text it was hiding. */}
      <span className={`status status-${state.status.kind}`}>
        {state.status.kind === "running" && state.activeTurnId !== null ? "working" : state.status.kind}
      </span>
      <span className="backend" title={`backend: ${state.backend}`}>{state.backend}</span>
      {/* Three identities where there really are three. The legacy backend's CLI never separated
          its own session id from Claude's, so `sessionId` and `providerSessionId` are the SAME
          value there -- printing both would invite a reader to conclude the two are distinct and
          happen to match, which is the opposite of true. Collapse it, and say whose id it is. */}
      <span
        className="identities"
        title={[
          `conversation (neovibe): ${state.conversationId ?? "n/a"}`,
          `session (verdandi):     ${state.sessionId ?? "not yet assigned"}`,
          `provider (claude):      ${state.providerSessionId ?? "not yet assigned"}`,
        ].join("\n")}
      >
        {state.conversationId !== null && <>conv {shortId(state.conversationId)} · </>}
        {state.sessionId !== null && state.sessionId === state.providerSessionId ? (
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
