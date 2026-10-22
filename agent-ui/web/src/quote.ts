/**
 * `>` in VISUAL/V-LINE:
 * quotes the highlighted text into the tab's draft, never sends anything. Pure functions -- `App.tsx`
 * owns the DOM read (the same text `y` would copy, through the same check and shield) and the
 * draft/mode/caret side effects; this module only formats and appends.
 */

/** The format: `\r\n`/`\r` become `\n`; leading and trailing blank lines are dropped; every
 *  remaining line becomes `> ` plus the line, and a line empty or only whitespace becomes a bare
 *  `>`; the lines are joined with `\n` and followed by `\n\n`, so a blank line always closes the
 *  quote (without it CommonMark would lazily continue a following paragraph into the blockquote).
 *  `null` when nothing is left after trimming -- the caller's "nothing to quote" flash, VISUAL/
 *  V-LINE stays on (a refusal distinct from `y`'s copy failure, which always ends the
 *  region). */
export function formatQuote(text: string): string | null {
  const normalized = text.replace(/\r\n/g, "\n").replace(/\r/g, "\n");
  const trimmed = normalized.replace(/^\n+/, "").replace(/\n+$/, "");
  if (trimmed.length === 0) return null;
  const lines = trimmed.split("\n").map((line) => (line.trim().length === 0 ? ">" : `> ${line}`));
  return `${lines.join("\n")}\n\n`;
}

/** The append rule: to the END of `draft`, never at the composer's caret. An empty draft
 *  takes `quote` as is (it already ends in `\n\n`, `formatQuote`'s own promise); a draft already
 *  ending in `\n\n` gets nothing between; one ending in a single `\n` gets one more; otherwise two.
 *  So repeated quotes stack in order, each its own blockquote, with exactly one blank line ahead of
 *  it and the two `formatQuote`'s own trailing `\n\n` leaves behind it. */
export function appendQuote(draft: string, quote: string): string {
  if (draft.length === 0) return quote;
  if (draft.endsWith("\n\n")) return draft + quote;
  if (draft.endsWith("\n")) return `${draft}\n${quote}`;
  return `${draft}\n\n${quote}`;
}
