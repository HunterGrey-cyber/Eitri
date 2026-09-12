import type { AgentUiState } from "../types";

function shortId(id: string | null): string {
  return id === null ? "—" : id.slice(0, 8);
}

export function SessionHeader({ state }: { state: AgentUiState }) {
  const provider = state.provider;
  return (
    <div className="session-header">
      <span className="model">{state.model ?? "no model yet"}</span>
      <span className={`status status-${state.status.kind}`}>
        {state.activeTurnId !== null ? "working" : state.status.kind}
      </span>
      <span className="backend" title={`backend: ${state.backend}`}>{state.backend}</span>
      {/* Three identities, shown as three. Hovering gives the full values -- the short forms are
          for glancing, but a session id you cannot read in full is a session id you cannot report
          in a bug. */}
      <span
        className="identities"
        title={[
          `conversation (neovibe): ${state.conversationId ?? "n/a"}`,
          `session (verdandi):     ${state.sessionId ?? "n/a"}`,
          `provider (claude):      ${state.providerSessionId ?? "not yet assigned"}`,
        ].join("\n")}
      >
        conv {shortId(state.conversationId)} · sess {shortId(state.sessionId)} · claude{" "}
        {shortId(state.providerSessionId)}
      </span>
      {provider !== null && (
        <span
          className="provider"
          title={[
            `sidecar ${provider.sidecarVersion} · protocol ${provider.protocol}`,
            `claude-agent-sdk ${provider.claudeAgentSdkVersion}`,
            `claude CLI ${provider.claudeCodeVersion}`,
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
