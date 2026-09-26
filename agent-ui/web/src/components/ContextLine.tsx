import type { ContextSummary } from "../types";

export function contextLineText(context: ContextSummary): string {
  if (context.lines === null) return `⧉ ${context.file}`;
  const [start, end] = context.lines;
  return start === end ? `⧉ ${context.file} · L${start}` : `⧉ ${context.file} · L${start}–${end}`;
}

/** V1: what rides along with the next turn (Claude Code's IDE-selection chip, Cursor's context chips). */
export function ContextLine({ context }: { context: ContextSummary | null }) {
  if (context === null || context.file === null) return null;
  return <div className="context-line">{contextLineText(context)}</div>;
}
