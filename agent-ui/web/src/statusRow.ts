import type { AgentUiState, ProviderInfo } from "./types";

/** The line `agent/src/providers/claude_sidecar/spawn.rs::describe_checkout` writes for version skew. */
export const SKEW_PREFIX = "Verdandi baseline drift";

/** D12 A: `⚠` only for version skew or a refusal (ruling 9). Returns what the glyph's title says, or
 *  null. Routine diagnostics (the CLI version one, present on every session on the owner's host) go
 *  to the detail popover only. */
export function statusWarning(provider: ProviderInfo | null, failure: string | null): string | null {
  if (failure !== null) return failure;
  return provider?.startupDiagnostics.find((line) => line.startsWith(SKEW_PREFIX)) ?? null;
}

/** Claude Code's statusLine, beneath the input (docs, statusline.md): model, backend, position. */
export function statusRowText(state: AgentUiState, position: { index: number; total: number }): string {
  const where = position.total === 0 ? "—" : `${position.index + 1}/${position.total}`;
  return `${state.model ?? "no model yet"} · ${state.backend} · ${where}`;
}
