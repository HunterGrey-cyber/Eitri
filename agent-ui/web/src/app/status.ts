import type { BandFacts } from "../band";
import type { ContextSummary, ProviderInfo } from "../types";

/** `⚠` only for an unavailable Claude Code prompt, a tab that could not run its sidecar update, or a
 *  refusal: a warning that shows on every session stops being read.
 *  Returns what the glyph's title says, or null. Routine diagnostics (the CLI version one, which can
 *  be present on every session of a host) go to the detail popover only. */
export function statusWarning(provider: ProviderInfo | null, failure: string | null): string | null {
  if (failure !== null) return failure;
  return provider?.startupDiagnostics.find(
    (line) => line.startsWith("Claude Code's system prompt unavailable") || line.startsWith("Sidecar update"),
  ) ?? null;
}

/** `ContextSummary` (the wire shape) to `BandFacts["context"]` (band.ts's own shape, `file`
 *  non-nullable once present): the one place both the empty tab's band and the conversation's own
 *  narrow the "no file" case to `null` rather than each repeating the check. */
export function contextFact(context: ContextSummary | null): BandFacts["context"] {
  if (context === null || context.file === null) return null;
  return { file: context.file, lines: context.lines };
}

