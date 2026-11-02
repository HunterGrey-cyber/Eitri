/** The project mention under the caret. Quotes keep spaces inside a single mention. */
export function atToken(draft: string, caret: number): { query: string; start: number; end: number } | null {
  if (draft.startsWith("!") || /^\s*\//.test(draft) || caret < 0 || caret > draft.length) return null;
  for (let start = 0; start < draft.length; start += 1) {
    if (draft[start] !== "@" || (start > 0 && !/\s/u.test(draft[start - 1]))) continue;
    const quoted = draft[start + 1] === '"';
    const from = start + (quoted ? 2 : 1);
    let end = from;
    while (end < draft.length && (quoted ? draft[end] !== '"' && draft[end] !== "\n" : !/\s/u.test(draft[end]))) end += 1;
    const queryEnd = end;
    if (quoted && draft[end] === '"') end += 1;
    if (caret > start && caret <= end) {
      return { query: draft.slice(from, Math.max(from, Math.min(caret, queryEnd))), start, end };
    }
    start = end - 1;
  }
  return null;
}

/** A directory keeps completing; a file leaves the caret after its separating space. */
export function atInsertion(path: string): { text: string; keepOpen: boolean } {
  const keepOpen = path.endsWith("/");
  const name = /\s/u.test(path) ? `"${path}"` : path;
  return { text: `@${name}${keepOpen ? "" : " "}`, keepOpen };
}
