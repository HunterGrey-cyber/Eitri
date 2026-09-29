/**
 * `>` in VISUAL/V-LINE quotes the highlighted text into the tab's draft without sending it.
 * `App.tsx` reads the DOM text through the same checks as `y` and manages draft, mode and caret
 * changes. This module only formats and appends text.
 */

/** Normalize `\r\n` and `\r` to `\n`, trim leading and trailing blank lines, and prefix each
 *  remaining line with `> `. Whitespace-only lines become bare `>`. Join with `\n` and append
 *  `\n\n` so CommonMark cannot lazily continue the next paragraph into the quote. Return `null`
 *  if trimming leaves nothing; the caller shows "nothing to quote" and keeps VISUAL/V-LINE
 *  active. */
export function formatQuote(text: string): string | null {
  const normalized = text.replace(/\r\n/g, "\n").replace(/\r/g, "\n");
  const trimmed = normalized.replace(/^\n+/, "").replace(/\n+$/, "");
  if (trimmed.length === 0) return null;
  const lines = trimmed.split("\n").map((line) => (line.trim().length === 0 ? ">" : `> ${line}`));
  return `${lines.join("\n")}\n\n`;
}

/** Append `quote` to the end of `draft`, regardless of the composer caret. An empty draft takes
 *  it unchanged. Otherwise insert enough newlines to separate it from the draft by a blank line:
 *  none after `\n\n`, one after `\n`, or two otherwise. `formatQuote` already supplies the
 *  trailing `\n\n`, so repeated quotes stay separate and in order. */
export function appendQuote(draft: string, quote: string): string {
  if (draft.length === 0) return quote;
  if (draft.endsWith("\n\n")) return draft + quote;
  if (draft.endsWith("\n")) return `${draft}\n${quote}`;
  return `${draft}\n\n${quote}`;
}
