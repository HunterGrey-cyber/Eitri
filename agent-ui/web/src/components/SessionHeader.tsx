import type { AgentUiState } from "../types";

export function SessionHeader({ state }: { state: AgentUiState }) {
  return (
    <div className="session-header">
      <span>{state.model ?? "no model yet"}</span>
      <span>{state.status.kind}</span>
    </div>
  );
}
