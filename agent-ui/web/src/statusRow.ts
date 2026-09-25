import type { AgentUiState, ProviderInfo } from "./types";

/** The line `agent/src/providers/claude_sidecar/spawn.rs::describe_checkout` writes for version skew. */
export const SKEW_PREFIX = "Verdandi baseline drift";

/** Title text for `⚠` on version skew or a refusal, or `null` when neither applies. Routine
 *  diagnostics, including the CLI version, appear only in the detail popover. */
export function statusWarning(provider: ProviderInfo | null, failure: string | null): string | null {
  if (failure !== null) return failure;
  return provider?.startupDiagnostics.find((line) => line.startsWith(SKEW_PREFIX)) ?? null;
}

/** Claude Code's statusLine, beneath the input (docs, statusline.md): model, backend, position. */
export function statusRowText(state: AgentUiState, position: { index: number; total: number }): string {
  const where = position.total === 0 ? "—" : `${position.index + 1}/${position.total}`;
  return `${state.model ?? "no model yet"} · ${state.backend} · ${where}`;
}
